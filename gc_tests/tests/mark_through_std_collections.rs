// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, DebugFlags, Debug, Gc};

struct GcData {
    a: Gc<usize>,
    b: Gc<usize>,
}

impl GcData {
    fn new(a: Gc<usize>, b: Gc<usize>) -> Self {
        Self { a, b }
    }
}

fn main() {
    gcmalloc::debug_flags(DebugFlags::new().sweep_phase(false));

    let mut gcs = Vec::new();
    for i in 1..100 {
        gcs.push(Gc::new(i))
    }

    gcmalloc::collect();

    for gc in gcs.iter() {
        assert!(Debug::is_black(Gc::into_raw(gc.clone()) as *mut u8));
    }
}
