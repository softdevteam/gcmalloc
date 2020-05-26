use std::{env, path::PathBuf, process::Command};

use lang_tester::LangTester;
use tempfile::TempDir;

static CRATE_NAME: &'static str = "gcmalloc";

fn libgcmalloc_path() -> String {
    let mut path = PathBuf::new();
    path.push(env::var("CARGO_MANIFEST_DIR").unwrap());
    path.push("target");
    #[cfg(debug_assertions)]
    path.push("debug");
    #[cfg(not(debug_assertions))]
    path.push("release");
    path.push("libgcmalloc.rlib");

    format!("{}={}", CRATE_NAME, path.to_str().unwrap()).to_owned()
}

fn deps_path() -> String {
    let mut path = PathBuf::new();
    path.push(env::var("CARGO_MANIFEST_DIR").unwrap());
    path.push("target");
    #[cfg(debug_assertions)]
    path.push("debug");
    #[cfg(not(debug_assertions))]
    path.push("release");
    path.push("deps");

    format!("dependency={}", path.to_str().unwrap()).to_owned()
}

fn main() {
    // We grab the rlibs from `target/<debug | release>/` but in order
    // for them to exist here, they must have been moved from `deps/`.
    // Simply running `cargo test` will not do this, instead, we must
    // ensure that `cargo build` has been run before running the tests.
    Command::new("cargo")
        .args(&["build"])
        .output()
        .expect("Failed to build libs");

    let tempdir = TempDir::new().unwrap();
    LangTester::new()
        .test_dir("gc_tests/tests")
        .test_file_filter(|p| {
            p.extension().unwrap().to_str().unwrap() == "rs"
                && !p
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("valgrind")
        })
        .test_extract(|s| {
            Some(
                s.lines()
                    .take_while(|l| l.starts_with("//"))
                    .map(|l| &l[2..])
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
        .test_cmds(move |p| {
            let mut exe = PathBuf::new();
            exe.push(&tempdir);
            exe.push(p.file_stem().unwrap());

            let mut compiler = Command::new("rustc");
            compiler.args(&[
                "--edition=2018",
                "-Zsanitizer=thread",
                "-o",
                exe.to_str().unwrap(),
                p.to_str().unwrap(),
                "--crate-name",
                CRATE_NAME,
                "-L",
                deps_path().as_str(),
                "--extern",
                libgcmalloc_path().as_str(),
            ]);

            vec![("Compiler", compiler), ("Run-time", Command::new(exe))]
        })
        .run();

    let tempdir = TempDir::new().unwrap();
    LangTester::new()
        .test_dir("gc_tests/tests")
        .test_file_filter(|p| p.extension().unwrap().to_str().unwrap() == "rs")
        .test_extract(|s| {
            Some(
                s.lines()
                    .take_while(|l| l.starts_with("//"))
                    .map(|l| &l[2..])
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
        .test_cmds(move |p| {
            let mut exe = PathBuf::new();
            exe.push(&tempdir);
            exe.push(p.file_stem().unwrap());

            let mut compiler = Command::new("rustc");
            compiler.args(&[
                "--edition=2018",
                "-g",
                "-o",
                exe.to_str().unwrap(),
                p.to_str().unwrap(),
                "--crate-name",
                CRATE_NAME,
                "-L",
                deps_path().as_str(),
                "--extern",
                libgcmalloc_path().as_str(),
            ]);

            let suppr_file = PathBuf::from("gc_tests/valgrind.supp");
            let suppressions = ["--suppressions=", suppr_file.to_str().unwrap()].concat();

            let mut runtime = Command::new("valgrind");
            runtime.args(&[
                "--error-exitcode=1",
                "--leak-check=no",
                suppressions.as_str(),
                exe.to_str().unwrap(),
            ]);

            vec![("Compiler", compiler), ("Run-time", runtime)]
        })
        .run();
}
