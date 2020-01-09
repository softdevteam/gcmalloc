// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, Debug, DebugFlags, Gc};

fn foo() {
    let y = Gc::new(456 as usize);
    collect();
    assert!(Debug::is_black(Gc::into_raw(y) as *mut u8));
}

fn main() {
    gcmalloc::debug_flags(DebugFlags::new().sweep_phase(false));

    let x = Gc::new(123 as usize);
    foo(); // triggers a collection
    assert!(Debug::is_black(Gc::into_raw(x) as *mut u8));
}
