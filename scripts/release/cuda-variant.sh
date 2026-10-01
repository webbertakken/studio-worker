#!/usr/bin/env bash
# Build, verify and package the CUDA variant of the release binary.
#
#   cuda-variant.sh build            cargo build --profile dist with the release features + cuda
#   cuda-variant.sh verify           check device code, linkage and that the binary starts
#   cuda-variant.sh package <dir>    write studio-worker-<target>-cuda.tar.xz (+ .sha256) to <dir>
#
# Docs: docs/operations/release.md#cuda-variant
set -euo pipefail

TARGET="${TARGET:-x86_64-unknown-linux-gnu}"
# Real device code for every supported generation, PTX of compute 9.0 for GPUs newer than the list.
#   61 Pascal (GTX 10xx)   70 Volta (V100)        75 Turing (RTX 20xx, T4)   80 Ampere (A100)
#   86 Ampere (RTX 30xx)   89 Ada (RTX 40xx, L4)  90 Hopper (H100)           120a Blackwell (RTX 50xx)
CUDA_ARCHS="${CUDA_ARCHS:-61-real;70-real;75-real;80-real;86-real;89-real;90-real;90-virtual;120a-real}"
EXPECTED_SASS="${EXPECTED_SASS-sm_61 sm_70 sm_75 sm_80 sm_86 sm_89 sm_90 sm_120a}"
EXPECTED_PTX="${EXPECTED_PTX-sm_90}"
PROFILE=dist
BIN="target/${TARGET}/${PROFILE}/studio-worker"
ARCHIVE_STEM="studio-worker-${TARGET}-cuda"
CUOBJDUMP="${CUDA_HOME:-/usr/local/cuda}/bin/cuobjdump"

log() { printf '[cuda-variant] %s\n' "$*" >&2; }
die() { printf '[cuda-variant] ERROR: %s\n' "$*" >&2; exit 1; }

release_features() {
  cargo metadata --format-version 1 --no-deps --locked |
    jq -r '.metadata.dist.features | join(",")'
}

build() {
  local features jobs started
  features="$(release_features),cuda"
  jobs="${JOBS:-$(nproc)}"
  # llama-cpp-sys-2 forwards CMAKE_* to CMake; CUDAARCHS is CMake's own default for the
  # same list.  Both only take effect on a fresh CMake build dir: `verify` checks the result.
  export CMAKE_CUDA_ARCHITECTURES="$CUDA_ARCHS" CUDAARCHS="$CUDA_ARCHS"
  log "target=${TARGET} features=${features} archs=${CUDA_ARCHS} jobs=${jobs}"
  started=$(date +%s)
  cargo build --profile "$PROFILE" --locked --target "$TARGET" --features "$features" --jobs "$jobs"
  log "build took $(( $(date +%s) - started ))s"
}

ggml_cuda_lib() {
  local lib
  lib="$(find "target/${TARGET}/${PROFILE}/build" -name 'libggml-cuda.a' -newer Cargo.lock -print -quit)"
  [ -n "$lib" ] || lib="$(find "target/${TARGET}/${PROFILE}/build" -name 'libggml-cuda.a' -print -quit)"
  [ -n "$lib" ] || die "libggml-cuda.a not found under target/${TARGET}/${PROFILE}/build"
  printf '%s\n' "$lib"
}

verify() {
  [ -x "$BIN" ] || die "$BIN is missing; run build first"
  local lib sass ptx arch linked stub_dir
  lib="$(ggml_cuda_lib)"

  # Device code: exactly the architectures asked for (a stale CMake cache keeps an old list).
  sass="$("$CUOBJDUMP" --list-elf "$lib" | grep -o 'sm_[0-9]*a\?' | sort -u | tr '\n' ' ')"
  ptx="$("$CUOBJDUMP" --list-ptx "$lib" | grep -o 'sm_[0-9]*a\?' | sort -u | tr '\n' ' ')"
  log "ggml-cuda device code: sass=[${sass% }] ptx=[${ptx% }]"
  for arch in $EXPECTED_SASS; do
    grep -qw "$arch" <<<"$sass" || die "no ${arch} device code in ${lib}"
  done
  for arch in $EXPECTED_PTX; do
    grep -qw "$arch" <<<"$ptx" || die "no ${arch} PTX in ${lib}"
  done
  [ "$(wc -w <<<"$sass")" -eq "$(wc -w <<<"$EXPECTED_SASS")" ] ||
    die "unexpected device code [${sass% }], expected [${EXPECTED_SASS}] (stale CMake cache?)"

  # Linkage: only the driver (libcuda.so.1) may be dynamic, so users need no CUDA toolkit.
  linked="$(readelf -d "$BIN" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p' | tr '\n' ' ')"
  log "dynamic libraries: ${linked% }"
  grep -qw 'libcuda.so.1' <<<"$linked" || die "binary does not link libcuda.so.1 (not a CUDA build?)"
  if grep -Eq 'libcudart|libcublas|libnvrtc' <<<"$linked"; then
    die "binary links CUDA toolkit libraries dynamically: ${linked}"
  fi

  # It starts: the driver stub stands in for libcuda.so.1 on a runner without a GPU.
  stub_dir="$(mktemp -d)"
  ln -s "${CUDA_HOME:-/usr/local/cuda}/lib64/stubs/libcuda.so" "$stub_dir/libcuda.so.1"
  LD_LIBRARY_PATH="$stub_dir" "$BIN" --version
  rm -rf "$stub_dir"
  log "binary size: $(stat -c %s "$BIN") bytes"
}

package() {
  local out="${1:?usage: package <dir>}" stage name started
  [ -x "$BIN" ] || die "$BIN is missing; run build first"
  mkdir -p "$out"
  stage="$(mktemp -d)"
  mkdir "$stage/$ARCHIVE_STEM"
  cp "$BIN" CHANGELOG.md LICENSE README.md "$stage/$ARCHIVE_STEM/"
  name="${ARCHIVE_STEM}.tar.xz"
  started=$(date +%s)
  # The layout cargo-dist uses: one top-level directory holding the binary and its docs.
  tar -C "$stage" -cf - "$ARCHIVE_STEM" | xz -T0 -6 >"$out/$name"
  rm -rf "$stage"
  (cd "$out" && sha256sum -b "$name" >"$name.sha256")
  log "packaged $out/$name: $(stat -c %s "$out/$name") bytes in $(( $(date +%s) - started ))s"
  cat "$out/$name.sha256"
}

case "${1:-}" in
  build) build ;;
  verify) verify ;;
  package) shift; package "$@" ;;
  *) die "usage: $0 build|verify|package <dir>" ;;
esac
