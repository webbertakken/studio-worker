/*
 * glibc 2.38 compatibility shims for the prebuilt ONNX Runtime static lib.
 *
 * glibc 2.38 introduced `__isoc23_*` symbol variants of strtol/scanf for C23
 * base-prefix handling.  The prebuilt onnxruntime (and its bundled protobuf /
 * abseil / nlohmann::json), compiled against glibc >= 2.38, references those
 * symbols.  Linking the resulting `libort_sys.a` on an older glibc — e.g. the
 * cargo-dist release runner (ubuntu-22.04, glibc 2.35) — fails with
 * "undefined symbol: __isoc23_strtoll".
 *
 * These shims forward to the classic functions and are declared **weak**, so:
 *   - on glibc < 2.38 (release runner) they fill the otherwise-undefined
 *     symbols, letting the static lib link;
 *   - on glibc >= 2.38 the real (strong) glibc symbols win and these objects
 *     are simply not pulled in — no duplicate-symbol clash.
 *
 * Compiled only for the `image-onnx` feature on Linux (see build.rs); a no-op
 * everywhere else.  The C23 vs classic difference (base-0 "0b" prefix parsing)
 * is irrelevant to onnxruntime's integer/format parsing here.
 */
#define _GNU_SOURCE
#include <inttypes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <wchar.h>

#define ISOC23_WEAK __attribute__((weak))

ISOC23_WEAK long __isoc23_strtol(const char *p, char **e, int b) { return strtol(p, e, b); }
ISOC23_WEAK long long __isoc23_strtoll(const char *p, char **e, int b) { return strtoll(p, e, b); }
ISOC23_WEAK unsigned long __isoc23_strtoul(const char *p, char **e, int b) { return strtoul(p, e, b); }
ISOC23_WEAK unsigned long long __isoc23_strtoull(const char *p, char **e, int b) {
  return strtoull(p, e, b);
}
ISOC23_WEAK intmax_t __isoc23_strtoimax(const char *p, char **e, int b) { return strtoimax(p, e, b); }
ISOC23_WEAK uintmax_t __isoc23_strtoumax(const char *p, char **e, int b) {
  return strtoumax(p, e, b);
}

ISOC23_WEAK long __isoc23_wcstol(const wchar_t *p, wchar_t **e, int b) { return wcstol(p, e, b); }
ISOC23_WEAK long long __isoc23_wcstoll(const wchar_t *p, wchar_t **e, int b) {
  return wcstoll(p, e, b);
}
ISOC23_WEAK unsigned long __isoc23_wcstoul(const wchar_t *p, wchar_t **e, int b) {
  return wcstoul(p, e, b);
}
ISOC23_WEAK unsigned long long __isoc23_wcstoull(const wchar_t *p, wchar_t **e, int b) {
  return wcstoull(p, e, b);
}

ISOC23_WEAK int __isoc23_vsscanf(const char *s, const char *f, va_list a) {
  return vsscanf(s, f, a);
}
ISOC23_WEAK int __isoc23_vfscanf(FILE *s, const char *f, va_list a) { return vfscanf(s, f, a); }
ISOC23_WEAK int __isoc23_vscanf(const char *f, va_list a) { return vscanf(f, a); }
ISOC23_WEAK int __isoc23_sscanf(const char *s, const char *f, ...) {
  va_list a;
  va_start(a, f);
  int r = vsscanf(s, f, a);
  va_end(a);
  return r;
}
ISOC23_WEAK int __isoc23_fscanf(FILE *s, const char *f, ...) {
  va_list a;
  va_start(a, f);
  int r = vfscanf(s, f, a);
  va_end(a);
  return r;
}
ISOC23_WEAK int __isoc23_scanf(const char *f, ...) {
  va_list a;
  va_start(a, f);
  int r = vscanf(f, a);
  va_end(a);
  return r;
}
