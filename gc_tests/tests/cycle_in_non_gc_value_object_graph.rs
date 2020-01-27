// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, Debug, DebugFlags, Gc};

struct S {
    x: usize,
    y: Gc<usize>,
}

fn main() {
    gcmalloc::debug_flags(DebugFlags::new().sweep_phase(false));

    let mut s = Box::new(S {
        x: 0,
        y: Gc::new(123),
    });

    // We take a reference to a non-garbage-collected heap object and place it
    // inside its own heap contents with the knowledge that the collector will
    // look through its fields and enqueue them for marking.
    //
    // If the original object hasn't been marked as "seen" before any of its
    // fields are popped off the queue and processed then we can end up in an
    // infinite loop.
    s.x = s.as_ref() as *const _ as usize;

    gcmalloc::collect();

    // If we get here, then the test has passed as it didn't get stuck in a
    // cycle.
}
