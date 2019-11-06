// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, gc::DebugFlags, Debug, Gc};

fn foo() {
    let y = Gc::new(456 as usize);
    collect();
    assert!(Debug::is_black(y));
}

fn main() {
    gcmalloc::init(DebugFlags::new().sweep_phase(false));

    let x = Gc::new(123 as usize);
    foo(); // triggers a collection
    assert!(Debug::is_black(x));
}
