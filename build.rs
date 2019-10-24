extern crate cc;

fn main() {
    cc::Build::new()
        .file("src/SpillRegisters_X64.S")
        .compile("libSpillRegisters.a");
}
