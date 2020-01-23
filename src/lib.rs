#![crate_name = "gcmalloc"]
#![crate_type = "rlib"]
#![feature(core_intrinsics)]
#![feature(allocator_api)]
#![feature(alloc_layout_extra)]
#![feature(raw_vec_internals)]
#![feature(const_fn)]
#![feature(coerce_unsized)]
#![feature(unsize)]
#![feature(maybe_uninit_ref)]
#[cfg(not(all(target_pointer_width = "64", target_arch = "x86_64")))]
compile_error!("Requires x86_64 with 64 bit pointer width.");

extern crate alloc as stdalloc;

extern crate packed_struct;
#[macro_use]
extern crate packed_struct_codegen;

#[macro_use]
extern crate static_assertions;

pub mod allocator;
pub mod collector;
pub mod gc;

pub use collector::DebugFlags;
pub use gc::Gc;

use crate::{
    allocator::{Block, GcAllocator, GlobalAllocator},
    collector::{Collector, CollectorPhase},
    gc::Colour,
};

use parking_lot::Mutex;

#[global_allocator]
static ALLOCATOR: GlobalAllocator = GlobalAllocator;

static mut GC_ALLOCATOR: GcAllocator = GcAllocator;

static COLLECTOR: Mutex<Collector> = Mutex::new(Collector::new());

static COLLECTOR_PHASE: Mutex<CollectorPhase> = Mutex::new(CollectorPhase::Ready);

/// The default number of GC values allocated before a collection is triggered.
const GC_ALLOCATION_THRESHOLD: usize = 100;

/// Perform a stop-the-world garbage collection. It is not recommended to call
/// this manually and instead let the collector decide when a collection is
/// necessary.
///
/// Calling `collect` when you believe that `Gc` managed values are no longer
/// used is not guaranteed to free those values and should not be relied upon.
pub fn collect() {
    COLLECTOR.lock().collect()
}

pub fn debug_flags(flags: DebugFlags) {
    COLLECTOR.lock().debug_flags = flags;
}

pub fn set_threshold(threshold: usize) {
    COLLECTOR.lock().allocation_threshold = threshold
}

/// Provides some useful functions for debugging and testing the collector.
pub struct Debug;

impl Debug {
    /// Returns true if the object was marked as reachable in the last collection.
    ///
    /// It can be misleading to check for the inverse of this function
    /// (`!is_black(..)`). It shouldn't be relied upon for testing, as
    /// conservative collectors tend to over-approximate and there are
    /// non-deterministic reasons that an unreachable object might still survive
    /// a collection: mis-identified integer, floating garbage in the red-zone,
    /// stale pointers in registers etc.
    pub fn is_black<T>(gc: *mut T) -> bool {
        assert_eq!(*COLLECTOR_PHASE.lock(), CollectorPhase::Ready);

        let block = Block::new(gc as *mut u8);
        block.colour() == Colour::Black
    }

    pub unsafe fn keep_alive<T>(gc: Gc<T>) {
        let mut block = Block::new(gc.ptr.as_ptr() as *mut u8);
        block.set_colour(Colour::Black);
    }
}
