//! Proves the host-probe layer (`sys.rs`) leaves operator-visible
//! tracing breadcrumbs.  Without these, a worker that silently reports
//! `0 GB VRAM` (because the NVIDIA sysfs tree isn't there, or the
//! sysfs file format changed) appears in production as "this worker
//! claims nothing" with zero log evidence pointing at the probe.
//!
//! Mirrors the per-module thread-isolated capture pattern from
//! `tests/http_errors.rs` / `tests/auto_update.rs` — see
//! `LESSONS_LEARNED.md` for why a fresh OS thread is required.

use std::io;
use std::sync::{Arc, Mutex};
use studio_worker::sys;
use tempfile::tempdir;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone)]
struct CapturingMakeWriter(Arc<Mutex<Vec<u8>>>);

struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for CapturingMakeWriter {
    type Writer = CapturingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CapturingWriter(self.0.clone())
    }
}

impl io::Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture<F: FnOnce() + Send + 'static>(f: F) -> String {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let buf_for_thread = buf.clone();
    std::thread::spawn(move || {
        let make_writer = CapturingMakeWriter(buf_for_thread);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(make_writer)
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, f);
    })
    .join()
    .expect("capture thread panicked");
    let bytes = buf.lock().unwrap().clone();
    String::from_utf8(bytes).expect("tracing output should be valid UTF-8")
}

// ---------------------------------------------------------------------------
// detect_vram_gb_from_sysfs — sysfs missing
// ---------------------------------------------------------------------------

#[test]
fn vram_probe_emits_info_with_source_no_nvidia_sysfs_when_root_missing() {
    let dir = tempdir().unwrap();
    // Path inside the tempdir that explicitly does not exist.
    let missing = dir.path().join("does-not-exist");
    let logs = capture(move || {
        let gb = sys::detect_vram_gb_from_sysfs(&missing);
        assert_eq!(gb, 0.0);
    });
    assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
    assert!(
        logs.contains("op=\"probe_vram\""),
        "expected op field, got: {logs}"
    );
    assert!(
        logs.contains("source=\"no_nvidia_sysfs\""),
        "expected source=no_nvidia_sysfs, got: {logs}"
    );
    assert!(
        logs.contains("vram_gb=0"),
        "expected vram_gb=0, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// detect_vram_gb_from_sysfs — sysfs present with a single GPU
// ---------------------------------------------------------------------------

#[test]
fn vram_probe_emits_info_with_gpu_count_and_total_when_sysfs_populated() {
    let dir = tempdir().unwrap();
    // Mimic /proc/driver/nvidia/gpus/<bus-id>/information layout.
    let gpu_dir = dir.path().join("0000:01:00.0");
    std::fs::create_dir_all(&gpu_dir).unwrap();
    std::fs::write(
        gpu_dir.join("information"),
        "Model:           NVIDIA Fake GPU\nVideo Memory:    24576 MiB\n",
    )
    .unwrap();
    let root = dir.path().to_path_buf();

    let logs = capture(move || {
        let gb = sys::detect_vram_gb_from_sysfs(&root);
        // 24576 MiB / 1024 = 24 GiB
        assert!((gb - 24.0).abs() < 1e-3, "expected ~24 GB, got {gb}");
    });
    assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
    assert!(
        logs.contains("op=\"probe_vram\""),
        "expected op field, got: {logs}"
    );
    assert!(
        logs.contains("source=\"nvidia_sysfs\""),
        "expected source=nvidia_sysfs, got: {logs}"
    );
    assert!(
        logs.contains("gpu_count=1"),
        "expected gpu_count=1, got: {logs}"
    );
    assert!(
        logs.contains("vram_gb=24"),
        "expected vram_gb=24, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// detect_vram_gb_from_sysfs — sysfs present but no `Video Memory` line
// (e.g. driver version bump changed the format).  We must NOT silently
// return 0 with the same `no_nvidia_sysfs` reason — the operator needs
// to know the directory was there but unparseable.
// ---------------------------------------------------------------------------

#[test]
fn vram_probe_emits_warn_when_sysfs_present_but_unparseable() {
    let dir = tempdir().unwrap();
    let gpu_dir = dir.path().join("0000:02:00.0");
    std::fs::create_dir_all(&gpu_dir).unwrap();
    std::fs::write(
        gpu_dir.join("information"),
        "Model:           NVIDIA Fake GPU\n",
    )
    .unwrap();
    let root = dir.path().to_path_buf();

    let logs = capture(move || {
        let gb = sys::detect_vram_gb_from_sysfs(&root);
        assert_eq!(gb, 0.0);
    });
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"probe_vram\""),
        "expected op field, got: {logs}"
    );
    assert!(
        logs.contains("source=\"sysfs_unparseable\""),
        "expected source=sysfs_unparseable, got: {logs}"
    );
    assert!(logs.contains("gpu_count=1"), "expected gpu_count=1: {logs}");
}

// ---------------------------------------------------------------------------
// machine_name / username — debug breadcrumb so a job that misbehaves
// can be correlated back to the host it ran on without tailing the
// process arguments.
// ---------------------------------------------------------------------------

#[test]
fn machine_name_emits_debug_event_with_value() {
    let logs = capture(|| {
        let _ = sys::machine_name();
    });
    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("op=\"machine_name\""),
        "expected op field, got: {logs}"
    );
    assert!(logs.contains("value="), "expected value field, got: {logs}");
}

#[test]
fn username_emits_debug_event_with_value() {
    let logs = capture(|| {
        let _ = sys::username();
    });
    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("op=\"username\""),
        "expected op field, got: {logs}"
    );
    assert!(logs.contains("value="), "expected value field, got: {logs}");
}
