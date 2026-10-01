#!/usr/bin/env bash
# Run the patched shell installer against a release layout on disk and check which build it
# installs for each STUDIO_WORKER_VARIANT.
#
#   test-installer.sh <distrib dir>          test a patched release layout (the release job)
#   test-installer.sh --fake <distrib dir>   stage fake x86_64 Linux archives beside cargo-dist's
#                                            global artifacts, patch the installer, then test
#
# The installer downloads from file://<distrib dir> (STUDIO_WORKER_DOWNLOAD_URL) and installs
# into a throwaway HOME, so nothing outside a temp dir changes.  x86_64 Linux only.
set -euo pipefail

TARGET=x86_64-unknown-linux-gnu
CPU_ARCHIVE="studio-worker-${TARGET}.tar.xz"
CUDA_ARCHIVE="studio-worker-${TARGET}-cuda.tar.xz"
HERE="$(cd "$(dirname "$0")" && pwd)"

die() { printf '[test-installer] FAIL: %s\n' "$*" >&2; exit 1; }
log() { printf '[test-installer] %s\n' "$*" >&2; }

[ "$(uname -s)-$(uname -m)" = "Linux-x86_64" ] || die "runs on x86_64 Linux only"

fake_archive() {
  local dir="$1" stem="$2" says="$3" stage
  stage="$(mktemp -d)"
  mkdir "$stage/$stem"
  # It says what it was asked to do; `setup` fails when FAKE_SETUP_FAILS is set.
  sed "s/@SAYS@/$says/" >"$stage/$stem/studio-worker" <<'FAKE'
#!/bin/sh
echo "@SAYS@: $*"
[ "$1" != setup ] || [ -z "${FAKE_SETUP_FAILS:-}" ] || exit 3
FAKE
  chmod +x "$stage/$stem/studio-worker"
  tar -C "$stage" -cJf "$dir/$stem.tar.xz" "$stem"
  rm -rf "$stage"
  (cd "$dir" && sha256sum -b "$stem.tar.xz" >"$stem.tar.xz.sha256")
}

# Real binaries are only ever installed as an update, so no test starts a real tray UI.
DEFAULT_UPDATE=1
if [ "${1:-}" = --fake ]; then
  DEFAULT_UPDATE=
  distrib="${2:?usage: test-installer.sh --fake <distrib dir>}"
  fake_archive "$distrib" "studio-worker-${TARGET}" "fake cpu build"
  fake_archive "$distrib" "studio-worker-${TARGET}-cuda" "fake cuda build"
  "$HERE/patch-installer.sh" "$distrib"
else
  distrib="${1:?usage: test-installer.sh [--fake] <distrib dir>}"
fi
distrib="$(cd "$distrib" && pwd)"
installer="$distrib/studio-worker-installer.sh"
for f in "$installer" "$distrib/$CPU_ARCHIVE" "$distrib/$CUDA_ARCHIVE"; do
  [ -f "$f" ] || die "$f not found"
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

binary_sha() {
  local out="$work/extract-$1"
  mkdir -p "$out"
  tar -C "$out" -xf "$distrib/$1" --strip-components 1
  sha256sum "$out/studio-worker" | awk '{ print $1 }'
}
cpu_sha="$(binary_sha "$CPU_ARCHIVE")"
cuda_sha="$(binary_sha "$CUDA_ARCHIVE")"
[ "$cpu_sha" != "$cuda_sha" ] || die "the CPU and CUDA archives hold the same binary"

# Run the installer from <download dir> with STUDIO_WORKER_VARIANT=<variant> (empty: unset),
# as an auto-update when <update> is 1 (default: DEFAULT_UPDATE).
# Sets $status, $output and $installed (the installed binary's sha256, or "none").
run_installer() {
  local variant="$1" from="$2" update="${3-$DEFAULT_UPDATE}" home="$work/home-$RANDOM"
  mkdir -p "$home"
  status=0
  output="$(
    env -i PATH="$PATH" HOME="$home" \
      ${variant:+STUDIO_WORKER_VARIANT="$variant"} \
      ${update:+STUDIO_WORKER_UPDATE=1} \
      ${FAKE_SETUP_FAILS:+FAKE_SETUP_FAILS=1} \
      STUDIO_WORKER_DOWNLOAD_URL="file://$from" \
      STUDIO_WORKER_INSTALL_DIR="$home/install" \
      STUDIO_WORKER_NO_MODIFY_PATH=1 \
      sh "$installer" 2>&1
  )" || status=$?
  # cargo-dist installs into <STUDIO_WORKER_INSTALL_DIR>/bin.
  if [ -f "$home/install/bin/studio-worker" ]; then
    installed="$(sha256sum "$home/install/bin/studio-worker" | awk '{ print $1 }')"
  else
    installed=none
  fi
}

# After an install the installer starts the tray UI (`setup`); for an update it does not.
expect_tray_start() {
  local label="$1"
  if [ -n "$DEFAULT_UPDATE" ]; then
    grep -qF "auto-update: the running tray UI restarts itself" <<<"$output" ||
      die "${label}: an update must say the tray UI restarts itself: ${output}"
    if grep -qF ": setup" <<<"$output"; then die "${label}: an update must not run setup: ${output}"; fi
  else
    if ! grep -qF "starting the studio-worker tray UI" <<<"$output" ||
      ! grep -qF "build: setup" <<<"$output"; then
      die "${label}: an install must run setup: ${output}"
    fi
  fi
}

expect_install() {
  local variant="$1" want_sha="$2" want_line="$3" label="${1:-unset}"
  run_installer "$variant" "$distrib"
  [ "$status" = 0 ] || die "STUDIO_WORKER_VARIANT=${label}: installer exited ${status}: ${output}"
  [ "$installed" = "$want_sha" ] || die "STUDIO_WORKER_VARIANT=${label}: installed the wrong build: ${output}"
  grep -qF "$want_line" <<<"$output" || die "STUDIO_WORKER_VARIANT=${label}: no '${want_line}' in: ${output}"
  expect_tray_start "$label"
  log "ok: STUDIO_WORKER_VARIANT=${label} installs ${want_line#variant: }"
}

# The installer must fail, install nothing and say <why>.
expect_refusal() {
  local variant="$1" from="$2" why="$3"
  run_installer "$variant" "$from"
  if [ "$status" = 0 ] || [ "$installed" != none ] || ! grep -qF "$why" <<<"$output"; then
    die "STUDIO_WORKER_VARIANT=$variant from $from must fail with '$why': ${output}"
  fi
}

has_driver() {
  local ldconfig
  for ldconfig in ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
    "$ldconfig" -p 2>/dev/null | grep -q 'libcuda\.so\.1 ' && return 0
  done
  for lib in /usr/lib/wsl/lib/libcuda.so.1 /usr/lib/x86_64-linux-gnu/libcuda.so.1 \
    /usr/lib64/libcuda.so.1 /usr/lib/libcuda.so.1; do
    [ -e "$lib" ] && return 0
  done
  return 1
}

expect_install cpu "$cpu_sha" "variant: cpu (STUDIO_WORKER_VARIANT=cpu)"
expect_install cuda "$cuda_sha" "variant: cuda (STUDIO_WORKER_VARIANT=cuda)"
if has_driver; then
  expect_install "" "$cuda_sha" "variant: cuda (NVIDIA driver found"
  expect_install auto "$cuda_sha" "variant: cuda (NVIDIA driver found"
else
  expect_install "" "$cpu_sha" "variant: cpu (no NVIDIA driver found)"
  expect_install auto "$cpu_sha" "variant: cpu (no NVIDIA driver found)"
  grep -qF "no NVIDIA driver (libcuda.so.1) found" <<<"$(run_installer cuda "$distrib"; echo "$output")" ||
    die "an explicit cuda install without a driver must warn"
  log "ok: an explicit cuda install without a driver warns"
fi

expect_refusal gpu "$distrib" "must be auto, cuda or cpu"
log "ok: an unknown STUDIO_WORKER_VARIANT fails"

# A CUDA archive that does not match the installer's checksum is refused.
tampered="$work/tampered"
mkdir "$tampered"
cp "$distrib/$CPU_ARCHIVE" "$distrib/$CUDA_ARCHIVE" "$tampered/"
printf 'x' >>"$tampered/$CUDA_ARCHIVE"
expect_refusal cuda "$tampered" "checksum"
log "ok: a tampered CUDA archive is refused"


if [ -z "$DEFAULT_UPDATE" ]; then
  # An auto-update installs without starting the tray UI: the running one restarts itself.
  DEFAULT_UPDATE=1 expect_install cpu "$cpu_sha" "variant: cpu (STUDIO_WORKER_VARIANT=cpu)"
  # A setup that fails warns with the command; the install itself stands.
  FAKE_SETUP_FAILS=1 run_installer cpu "$distrib"
  if [ "$status" != 0 ] || [ "$installed" != "$cpu_sha" ] ||
    ! grep -qF "could not start the tray UI; run:" <<<"$output"; then
    die "a failed setup must warn and keep the install: ${output}"
  fi
  log "ok: a failed setup warns and keeps the install"
fi

# The PowerShell installer parses and starts the tray UI after installing.
ps="$distrib/studio-worker-installer.ps1"
grep -qF "  Start-StudioWorkerTray \$dest_dir" "$ps" || die "$ps does not start the tray UI"
if command -v pwsh >/dev/null; then
  pwsh -NoProfile -Command "\$e = \$null; [void][System.Management.Automation.Language.Parser]::ParseFile('$ps', [ref]\$null, [ref]\$e); if (\$e) { \$e; exit 1 }" ||
    die "$ps does not parse"
  log "ok: the PowerShell installer parses and starts the tray UI"
else
  log "pwsh not installed: the PowerShell installer was checked for the call only"
fi
log "all installer checks passed"
