//! Build script.
//!
//! Compiles the glibc 2.38 `__isoc23_*` compatibility shims into the binary for
//! the `image-onnx` Linux build, so the prebuilt ONNX Runtime static lib (built
//! against glibc >= 2.38) links on the older-glibc cargo-dist release runner.
//! No-op for every other feature/target combination.

fn main() {
    let onnx = std::env::var_os("CARGO_FEATURE_IMAGE_ONNX").is_some();
    let linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    if onnx && linux {
        cc::Build::new()
            .file("src/glibc_compat.c")
            .flag_if_supported("-std=gnu11")
            .compile("glibc_compat");
        println!("cargo:rerun-if-changed=src/glibc_compat.c");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
