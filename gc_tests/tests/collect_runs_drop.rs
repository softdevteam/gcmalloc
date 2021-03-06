// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{DebugFlags, Gc};

static mut COUNTER: usize = 0;

struct IncrOnDrop(usize);

impl Drop for IncrOnDrop {
    fn drop(&mut self) {
        unsafe { COUNTER += 1 }
    }
}

fn main() {
    gcmalloc::debug_flags(DebugFlags::new().mark_phase(false));

    Gc::new(IncrOnDrop(123));

    gcmalloc::collect();

    unsafe { assert_eq!(COUNTER, 1) }
}
