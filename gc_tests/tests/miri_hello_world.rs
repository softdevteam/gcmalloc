// Miri:
//  status: success

extern crate gcmalloc;

use gcmalloc::Gc;

fn main() {
    let hello = Gc::new("Hello World");
    println!("{}", hello)
}
