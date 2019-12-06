// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{gc::DebugFlags, Gc};

static mut COUNTER: usize = 0;

struct HasInnerGc(Gc<IncrOnDrop>);

struct IncrOnDrop(Option<Box<IncrOnDrop>>);

impl Drop for IncrOnDrop {
    fn drop(&mut self) {
        unsafe { COUNTER += 1 }
    }
}

fn main() {
    gcmalloc::debug_flags(DebugFlags::new().mark_phase(false));

    let s = HasInnerGc(Gc::new(IncrOnDrop(None)));
    Gc::new(s);

    gcmalloc::collect();

    unsafe { assert_eq!(COUNTER, 1) }
}
