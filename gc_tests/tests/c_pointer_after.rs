// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, Debug, DebugFlags, Gc};

#[inline(never)]
fn setup_obj() -> Box<*mut u8> {
    let a = Gc::new(123_u8);

    // Return a pointer to 1 byte past the end of the value. This is valid in
    // the C (and Rust) standard, so it should keep the object alive.
    //
    // We box it to prevent the original pointer rooting the object through a
    // stale register across the call
    Box::new(unsafe { Gc::into_raw(a).add(1) as *mut u8 })
}

#[inline(never)]
fn check_marked(ptr: Box<*mut u8>) {
    // Reconstruct the base pointer to see if it was marked
    let base = unsafe { (*ptr).sub(1) as *mut u8 };
    assert!(Debug::is_black(base));
}

fn main() {
    gcmalloc::debug_flags(DebugFlags::new().sweep_phase(false));

    let a = setup_obj();
    collect();

    check_marked(a);
}
