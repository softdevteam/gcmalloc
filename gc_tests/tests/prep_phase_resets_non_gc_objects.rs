// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, Debug, DebugFlags, Gc};

fn main() {
    gcmalloc::debug_flags(DebugFlags::new().sweep_phase(false));

    let x = Box::new(Gc::new(123));

    // The first collection should colour x black.
    gcmalloc::collect();

    assert!(Debug::is_black(x.as_ref() as *const _ as *mut u8));

    // The preparation phase for the second collection should reset x to white.
    // If it doesn't, then values could be missed.
    gcmalloc::debug_flags(DebugFlags::new().sweep_phase(false).mark_phase(false));
    gcmalloc::collect();

    assert!(!Debug::is_black(x.as_ref() as *const _ as *mut u8));
}
