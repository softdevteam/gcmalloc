// Copyright (c) 2019 King's College London created by the Software Development
// Team <http://soft-dev.org/>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0>, or the MIT license <LICENSE-MIT
// or http://opensource.org/licenses/MIT>, or the UPL-1.0 license
// <http://opensource.org/licenses/UPL> at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::{env, path::PathBuf, process::Command};

use lang_tester::LangTester;
use tempdir::TempDir;

static CRATE_NAME: &'static str = "gcmalloc";

fn main() {
    let sanitizers: Vec<String> = match env::var("SANITIZERS") {
        Ok(ref s) => s.split(';').map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    };

    let valgrind: bool = match env::var("VALGRIND") {
        Ok(ref s) => s == "true",
        Err(_) => false,
    };

    // We grab the rlibs from `target/<debug | release>/` but in order
    // for them to exist here, they must have been moved from `deps/`.
    // Simply running `cargo test` will not do this, instead, we must
    // ensure that `cargo build` has been run before running the tests.
    Command::new("cargo")
        .args(&["build"])
        .output()
        .expect("Failed to build libs");

    let tempdir = TempDir::new("gc_tester").unwrap();
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
            // Test command 1: Compile `x.rs` into `tempdir/x`.
            let mut exe = PathBuf::new();
            exe.push(&tempdir);
            exe.push(p.file_stem().unwrap());
            let mut compiler = Command::new("rustc");

            let mut lib_path = PathBuf::new();
            lib_path.push(env::var("CARGO_MANIFEST_DIR").unwrap());
            lib_path.push("target");
            #[cfg(debug_assertions)]
            lib_path.push("debug");
            #[cfg(not(debug_assertions))]
            lib_path.push("release");
            lib_path.push("libgcmalloc.rlib");

            let mut deps_path = PathBuf::new();
            deps_path.push(env::var("CARGO_MANIFEST_DIR").unwrap());
            deps_path.push("target");
            #[cfg(debug_assertions)]
            deps_path.push("debug");
            #[cfg(not(debug_assertions))]
            deps_path.push("release");
            deps_path.push("deps");

            let lib_arg = [CRATE_NAME, "=", lib_path.to_str().unwrap()].concat();
            let deps_arg = ["dependency=", deps_path.to_str().unwrap()].concat();

            let san_flags = sanitizers.iter().map(|x| format!("-Zsanitizer={}", x));
            compiler.args(san_flags);

            compiler.args(&[
                "--edition=2018",
                "-o",
                exe.to_str().unwrap(),
                p.to_str().unwrap(),
                "--crate-name",
                CRATE_NAME,
                "-L",
                deps_arg.as_str(),
                "--extern",
                lib_arg.as_str(),
            ]);

            assert!(
                !(valgrind && !sanitizers.is_empty()),
                "Valgrind can't be used on code compiled with sanitizers"
            );

            let mut runtime;
            if valgrind {
                let suppr_file = PathBuf::from("gc_tests/valgrind.supp");
                let suppressions = ["--suppressions=", suppr_file.to_str().unwrap()].concat();
                runtime = Command::new("valgrind");
                runtime.args(&[
                    "--error-exitcode=1",
                    suppressions.as_str(),
                    exe.to_str().unwrap(),
                ]);
            } else {
                runtime = Command::new(exe);
            };

            vec![("Compiler", compiler), ("Run-time", runtime)]
        })
        .run();
}
