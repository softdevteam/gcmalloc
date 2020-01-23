// Run-time:
//   status: success

extern crate gcmalloc;

use gcmalloc::{collect, Debug, DebugFlags, Gc};
use std::alloc::Layout;

static mut COUNTER: usize = 0;

struct Inner;

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe { COUNTER += 1 }
    }
}

struct Large([usize; 10]);

fn main() {
    // Ensure that collection will always reclaim the object.
    gcmalloc::debug_flags(DebugFlags::new().mark_phase(false));

    let layout = Layout::from_size_align(8, 8).unwrap();

    // First, check that we can't create a Gc value for a layout smaller than T.
    let l = Gc::<Large>::new_from_layout(layout);
    assert!(l.is_none());

    // Since we obtain a Gc<MaybeUninit<T>>, no drop should be called on T if it
    // was never initialized.
    let x = Gc::<Inner>::new_from_layout(layout).unwrap();
    gcmalloc::collect();
    unsafe { assert_eq!(COUNTER, 0) }

    // Calling `assume_init()` will promote a Gc<MaybeUninit<T> to a Gc<T>,
    // meaning that T's drop method should now be called upon collection.
    let x = Gc::<Inner>::new_from_layout(layout).unwrap();
    unsafe { x.assume_init() };
    gcmalloc::collect();
    unsafe { assert_eq!(COUNTER, 1) }

    unsafe {
        let x = Gc::<Inner>::new_from_layout(layout).unwrap();
        let y = x.clone().assume_init();
        let z = x.clone().assume_init();
        x.assume_init();

        // We can do this multiple times, but the end result should still be
        // that the underlying Gc value is only dropped once upon collection.
        gcmalloc::collect();
        assert_eq!(COUNTER, 2);
    }
}
