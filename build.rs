extern crate cc;

use rerun_except::rerun_except;

fn main() {
    rerun_except(&["gc_tests/tests/*.rs"]).unwrap();

    cc::Build::new()
        .file("src/SpillRegisters_X64.S")
        .compile("libSpillRegisters.a");
}
