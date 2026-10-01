#!/usr/bin/env bash
# Teach cargo-dist's installers what it cannot generate: the shell installer's CUDA variant,
# the CUDA archives in sha256.sum, and, in both installers, starting the tray UI (`setup`).
#
#   patch-installer.sh <distrib dir>
#
# <distrib dir> holds cargo-dist's global artifacts (studio-worker-installer.sh, sha256.sum)
# and, for every target in CUDA_TARGETS, studio-worker-<target>-cuda.tar.xz{,.sha256}.
#
# The patched installer installs the CUDA archive on <target> when STUDIO_WORKER_VARIANT=cuda,
# or when it is unset/auto and the NVIDIA driver (libcuda.so.1) is present; the CPU archive
# otherwise.  It anchors on cargo-dist 0.30's generated text and fails if any anchor is
# missing, so a cargo-dist upgrade that moves them stops the release instead of shipping an
# installer that silently ignores the variant.  Docs: docs/operations/release.md#cuda-variant
#
# Both installers run `<install dir>/studio-worker setup` after installing (it starts the tray
# UI), except when STUDIO_WORKER_UPDATE=1: the auto-updater sets it, and the running tray UI
# restarts itself on the new binary.  Docs: docs/runtime/daemon-and-tray.md#install
set -euo pipefail

CUDA_TARGETS="${CUDA_TARGETS:-x86_64-unknown-linux-gnu}"

die() { printf '[patch-installer] ERROR: %s\n' "$*" >&2; exit 1; }
log() { printf '[patch-installer] %s\n' "$*" >&2; }

distrib="${1:?usage: patch-installer.sh <distrib dir>}"
installer="$distrib/studio-worker-installer.sh"
[ -f "$installer" ] || die "$installer not found"
ps_installer="$distrib/studio-worker-installer.ps1"
[ -f "$ps_installer" ] || die "$ps_installer not found"
grep -q 'STUDIO_WORKER_UPDATE' "$ps_installer" && die "$ps_installer is already patched"
grep -q 'STUDIO_WORKER_VARIANT' "$installer" && die "$installer is already patched"

# Count the lines equal to $2 in file $1.
count_lines() { awk -v want="$2" '$0 == want { n++ } END { print n + 0 }' "$1"; }

# Fail unless file $1 has exactly one line equal to $2.
expect_once() {
  local n
  n="$(count_lines "$1" "$2")"
  [ "$n" = 1 ] || die "anchor '$2' found $n times in $1 (cargo-dist changed its installer?)"
}

# 1. Helpers, defined before select_archive_for_arch.  They print to stderr: the caller
#    captures select_archive_for_arch's stdout as the archive name.
expect_once "$installer" 'select_archive_for_arch() {'
expect_once "$installer" '    local _archive'
expect_once "$installer" "    say \"everything's installed!\""
helpers="$(mktemp)"
cat >"$helpers" <<EOF
# studio-worker: the CUDA variant (STUDIO_WORKER_VARIANT=auto|cuda|cpu, default auto).
STUDIO_WORKER_CUDA_TARGETS="${CUDA_TARGETS}"

studio_worker_nvidia_driver() {
    # The CUDA build links libcuda.so.1, which the NVIDIA driver installs.
    local _ldconfig
    for _ldconfig in ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
        if "\$_ldconfig" -p 2>/dev/null | grep -q 'libcuda\.so\.1 '; then
            return 0
        fi
    done
    local _lib
    for _lib in /usr/lib/wsl/lib/libcuda.so.1 /usr/lib/x86_64-linux-gnu/libcuda.so.1 \\
        /usr/lib64/libcuda.so.1 /usr/lib/libcuda.so.1; do
        if [ -e "\$_lib" ]; then
            return 0
        fi
    done
    return 1
}

studio_worker_check_variant() {
    case "\${STUDIO_WORKER_VARIANT:-auto}" in
        auto|cpu|cuda) ;;
        *) err "STUDIO_WORKER_VARIANT must be auto, cuda or cpu, not '\$STUDIO_WORKER_VARIANT'" ;;
    esac
    if [ "\${STUDIO_WORKER_VARIANT:-auto}" = cuda ]; then
        case " \$STUDIO_WORKER_CUDA_TARGETS " in
            *" \$1 "*) ;;
            *) warn "no CUDA build for \$1: installing the CPU build" ;;
        esac
    fi
}

studio_worker_wants_cuda() {
    case "\${STUDIO_WORKER_VARIANT:-auto}" in
        cuda)
            say "variant: cuda (STUDIO_WORKER_VARIANT=cuda)" >&2
            if ! studio_worker_nvidia_driver; then
                warn "no NVIDIA driver (libcuda.so.1) found: the CUDA build starts once one is installed"
            fi
            return 0
            ;;
        cpu)
            say "variant: cpu (STUDIO_WORKER_VARIANT=cpu)" >&2
            return 1
            ;;
        *)
            if studio_worker_nvidia_driver; then
                say "variant: cuda (NVIDIA driver found; STUDIO_WORKER_VARIANT=cpu installs the CPU build)" >&2
                return 0
            fi
            say "variant: cpu (no NVIDIA driver found)" >&2
            return 1
            ;;
    esac
}

studio_worker_start_tray() {
    if [ "\${STUDIO_WORKER_UPDATE:-0}" = "1" ]; then
        say "auto-update: the running tray UI restarts itself on the new binary"
        return 0
    fi
    say "starting the studio-worker tray UI (studio-worker setup)"
    if ! "\$1/studio-worker" setup; then
        warn "could not start the tray UI; run: \$1/studio-worker setup"
    fi
}

EOF

patched="$(mktemp)"
everything_installed="    say \"everything's installed!\""
awk -v helpers="$helpers" -v everything="$everything_installed" '
  $0 == everything {
    print; print "    studio_worker_start_tray \"$_install_dir\""; next
  }
  $0 == "select_archive_for_arch() {" {
    while ((getline line < helpers) > 0) print line
    print; in_select = 1; next
  }
  in_select && $0 == "    local _archive" {
    print; print "    studio_worker_check_variant \"$1\""; in_select = 0; next
  }
  { print }
' "$installer" >"$patched"

# 2. Per target: try the CUDA archive first in select_archive_for_arch, and know its checksum.
for target in $CUDA_TARGETS; do
  cpu_archive="studio-worker-${target}.tar.xz"
  cuda_archive="studio-worker-${target}-cuda.tar.xz"
  [ -f "$distrib/$cuda_archive" ] || die "$distrib/$cuda_archive not found"
  [ -f "$distrib/$cuda_archive.sha256" ] || die "$distrib/$cuda_archive.sha256 not found"
  sha="$(awk '{ print $1; exit }' "$distrib/$cuda_archive.sha256")"
  [ "$(sha256sum "$distrib/$cuda_archive" | awk '{ print $1 }')" = "$sha" ] ||
    die "$cuda_archive does not match its .sha256"

  expect_once "$patched" "        \"${target}\")"
  expect_once "$patched" "            _archive=\"${cpu_archive}\""
  expect_once "$patched" "        \"${cpu_archive}\")"

  next="$(mktemp)"
  awk -v target="$target" -v cpu="$cpu_archive" -v cuda="$cuda_archive" -v sha="$sha" '
    # select_archive_for_arch: copy the CPU candidate (archive, glibc check, accept) for the
    # CUDA archive, gated on the variant, and put it first.
    $0 == "        \"" target "\")" { print; in_arch = 1; next }
    in_arch == 1 && $0 == "            _archive=\"" cpu "\"" {
      cand[n = 1] = $0; in_arch = 2; next
    }
    in_arch == 2 {
      cand[++n] = $0
      if ($0 == "                return 0") { in_arch = 3 }
      next
    }
    in_arch == 3 {
      cand[++n] = $0
      print "            _archive=\"" cuda "\""
      print "            if ! studio_worker_wants_cuda; then"
      print "                _archive=\"\""
      print "            fi"
      for (i = 2; i <= n; i++) {
        line = cand[i]
        if (line ~ /^            if ! check_glibc /) sub(/^            if /, "            if [ -n \"$_archive\" ] \\&\\& ", line)
        print line
      }
      for (i = 1; i <= n; i++) print cand[i]
      in_arch = 0; next
    }
    # The destructuring case: the CUDA archive is the CPU entry with its own name and, always,
    # its own checksum (cargo-dist omits checksums it does not know).
    $0 == "        \"" cpu "\")" { entry[m = 1] = $0; in_entry = 1; next }
    in_entry {
      entry[++m] = $0
      if ($0 == "            ;;") {
        print "        \"" cuda "\")"
        for (i = 2; i <= m; i++) {
          line = entry[i]
          if (line ~ /^            _checksum_(style|value)=/) continue
          print line
          if (line ~ /^            _zip_ext=/) {
            print "            _checksum_style=\"sha256\""
            print "            _checksum_value=\"" sha "\""
          }
        }
        for (i = 1; i <= m; i++) print entry[i]
        in_entry = 0
      }
      next
    }
    { print }
  ' "$patched" >"$next"
  mv "$next" "$patched"

  expect_once "$patched" "            _archive=\"${cuda_archive}\""
  expect_once "$patched" "        \"${cuda_archive}\")"
  expect_once "$patched" "            _checksum_value=\"${sha}\""

  if [ -f "$distrib/sha256.sum" ]; then
    grep -qF "$cuda_archive" "$distrib/sha256.sum" || cat "$distrib/$cuda_archive.sha256" >>"$distrib/sha256.sum"
  fi
  log "installer offers ${cuda_archive} (sha256 ${sha}) on ${target}"
done

expect_once "$patched" "    studio_worker_start_tray \"\$_install_dir\""
sh -n "$patched" || die "patched installer is not valid sh"
cat "$patched" >"$installer"
rm -f "$patched" "$helpers"
log "patched $installer"

# 3. The PowerShell installer starts the tray UI after installing, unless it runs for an update.
ps_function="$(mktemp)"
cat >"$ps_function" <<'EOF'
# studio-worker: start the tray UI after installing (docs/runtime/daemon-and-tray.md#install).
function Start-StudioWorkerTray($dest_dir) {
  if ($env:STUDIO_WORKER_UPDATE -eq "1") {
    Write-Information "auto-update: the running tray UI restarts itself on the new binary"
    return
  }
  Write-Information "starting the studio-worker tray UI (studio-worker setup)"
  $exe = Join-Path $dest_dir "studio-worker.exe"
  try {
    & $exe setup
    if ($LASTEXITCODE -ne 0) {
      Write-Warning "could not start the tray UI; run: $exe setup"
    }
  } catch {
    Write-Warning "could not start the tray UI ($_); run: $exe setup"
  }
}

EOF
ps_everything="  Write-Information \"everything's installed!\""
expect_once "$ps_installer" "function Install-Binary(\$install_args) {"
expect_once "$ps_installer" "$ps_everything"
ps_patched="$(mktemp)"
awk -v fn="$ps_function" -v everything="$ps_everything" '
  $0 == "function Install-Binary($install_args) {" {
    while ((getline line < fn) > 0) print line
    print; next
  }
  $0 == everything { print; print "  Start-StudioWorkerTray $dest_dir"; next }
  { print }
' "$ps_installer" >"$ps_patched"
expect_once "$ps_patched" "  Start-StudioWorkerTray \$dest_dir"
expect_once "$ps_patched" "function Start-StudioWorkerTray(\$dest_dir) {"
cat "$ps_patched" >"$ps_installer"
rm -f "$ps_patched" "$ps_function"
log "patched $ps_installer"
