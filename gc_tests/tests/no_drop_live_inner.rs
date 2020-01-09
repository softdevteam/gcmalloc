// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{Debug, DebugFlags, Gc};

static mut COUNTER: usize = 0;

struct HasInnerGc(Gc<IncrOnDrop>);

struct IncrOnDrop(Option<Box<IncrOnDrop>>);

impl Drop for IncrOnDrop {
    fn drop(&mut self) {
        unsafe { COUNTER += 1 }
    }
}

// This tests that if an inner Gc is kept alive from some reference outside a
// dying outer Gc, the inner destructor should *not* be ran.
fn main() {
    gcmalloc::debug_flags(DebugFlags::new().prep_phase(false).mark_phase(false));

    let inner = Gc::new(IncrOnDrop(None));
    unsafe { Debug::keep_alive(inner) };
    let outer = Gc::new((IncrOnDrop(None), HasInnerGc(inner)));

    gcmalloc::collect();

    unsafe { assert_eq!(COUNTER, 1) }
}
