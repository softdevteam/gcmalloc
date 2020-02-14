use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use lang_tester::LangTester;
use tempdir::TempDir;

fn proj_dir() -> Box<Path> {
    let mut p = PathBuf::new();
    p.push(env::var("CARGO_MANIFEST_DIR").unwrap());
    p.into_boxed_path()
}

fn test_dir() -> Box<Path> {
    let mut p = PathBuf::new();
    p.push(env::var("CARGO_MANIFEST_DIR").unwrap());
    p.push("gc_tests");
    p.push("tests");
    p.push(".");
    p.into_boxed_path()
}

fn main() {
    let projd = proj_dir();
    let testsd = test_dir();

    let tmpd = TempDir::new("miri_tests").unwrap();
    let mut binsd = PathBuf::new();
    binsd.push(tmpd.path());
    binsd.push("src");
    binsd.push("bin");

    let mut setup = Command::new("./gc_tests/miri_test_setup.sh")
        .args(&[&*projd, tmpd.path()])
        .spawn()
        .unwrap();

    if !setup.wait().unwrap().success() {
        panic!("Miri setup failed");
    }

    ::std::env::set_current_dir(&tmpd).expect("Couldn't change dir");

    LangTester::new()
        .test_dir(testsd.to_str().unwrap())
        .test_file_filter(|p| {
            p.extension().unwrap().to_str().unwrap() == "rs"
                && p.file_stem().unwrap().to_str().unwrap().starts_with("miri")
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
            let mut cp = Command::new("cp");
            cp.args(&[p, binsd.as_path()]);

            let mut runtime = Command::new("cargo-miri");
            runtime.args(&["miri", "run", "--", "-Zmiri-ignore-leaks"]);

            let mut rm_path = PathBuf::new();
            rm_path.push(&binsd);
            rm_path.push(p.file_name().unwrap().to_str().unwrap());

            let mut rm = Command::new("rm");
            rm.args(&[rm_path.to_str().unwrap()]);

            vec![("cp", cp), ("Miri", runtime), ("rm", rm)]
        })
        .run();
}
