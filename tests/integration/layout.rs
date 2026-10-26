//! Keeps the integration suite a single binary: no `tests/*.rs` files,
//! and every module file here declared in `main.rs`.

use std::fs;
use std::path::{Path, PathBuf};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn rust_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    names.sort();
    names
}

#[test]
fn no_test_files_outside_the_integration_binary() {
    let stray = rust_files(&tests_dir());
    assert!(
        stray.is_empty(),
        "each tests/*.rs file links a separate test binary; move {stray:?} into \
         tests/integration/ and declare it in tests/integration/main.rs"
    );
}

#[test]
fn every_integration_module_is_declared() {
    let dir = tests_dir().join("integration");
    let main = fs::read_to_string(dir.join("main.rs")).unwrap();
    let undeclared: Vec<String> = rust_files(&dir)
        .into_iter()
        .filter(|name| name != "main.rs")
        .filter(|name| {
            let module = name.trim_end_matches(".rs");
            !main
                .lines()
                .any(|line| line.trim() == format!("mod {module};"))
        })
        .collect();
    assert!(
        undeclared.is_empty(),
        "{undeclared:?} in tests/integration/ never compile; declare them in main.rs"
    );
}
