//! Proves the host-probe layer (`sys.rs`) leaves operator-visible
//! tracing breadcrumbs.  Without these, a worker that silently reports
//! `0 GB VRAM` (because the NVIDIA sysfs tree isn't there, or the
//! sysfs file format changed) appears in production as "this worker
//! claims nothing" with zero log evidence pointing at the probe.
//!
//! Uses the shared `studio_worker::test_support::capture` helper,
//! which installs one process-global subscriber + thread-local sink.

use studio_worker::sys;
use studio_worker::test_support::capture;
use tempfile::tempdir;

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
// detect_vram_gb_from_sysfs — multi-GPU box where one card parses and a
// second is present but unparseable (older driver / a card that lost its
// `Video Memory` line).  The survivor must still total, but the dropped
// card must leave a per-GPU WARN naming it and bump the summary's
// `dropped` count — otherwise the box silently under-reports its VRAM
// and refuses jobs it could actually run, with no log evidence.
// ---------------------------------------------------------------------------

#[test]
fn vram_probe_warns_and_counts_a_partially_dropped_gpu() {
    let dir = tempdir().unwrap();
    let good = dir.path().join("0000:01:00.0");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(
        good.join("information"),
        "Model: NVIDIA Fake GPU\nVideo Memory:    24576 MiB\n",
    )
    .unwrap();
    let bad = dir.path().join("0000:02:00.0");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("information"), "Model: NVIDIA Fake GPU\n").unwrap();
    let root = dir.path().to_path_buf();

    let logs = capture(move || {
        let gb = sys::detect_vram_gb_from_sysfs(&root);
        // Only the healthy 24 GiB card counts; the other is dropped.
        assert!((gb - 24.0).abs() < 1e-3, "expected ~24 GB, got {gb}");
    });
    // The success breadcrumb still fires (one GPU parsed) and now reports
    // the drop so a partial total can't pass for a complete one.
    assert!(
        logs.contains("source=\"nvidia_sysfs\""),
        "expected source=nvidia_sysfs, got: {logs}"
    );
    assert!(
        logs.contains("gpu_count=1"),
        "one GPU contributed, got: {logs}"
    );
    assert!(
        logs.contains("dropped=1"),
        "the breadcrumb must report the drop, got: {logs}"
    );
    // The dropped GPU is named in its own WARN with the reason.
    assert!(
        logs.contains("WARN"),
        "expected a per-GPU WARN, got: {logs}"
    );
    assert!(
        logs.contains("reason=\"no_video_memory_line\""),
        "expected the drop reason, got: {logs}"
    );
    assert!(
        logs.contains("0000:02:00.0"),
        "the warn must name the dropped GPU, got: {logs}"
    );
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
