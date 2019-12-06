#![crate_name = "gcmalloc"]
#![crate_type = "rlib"]
#![feature(core_intrinsics)]
#![feature(allocator_api)]
#![feature(alloc_layout_extra)]
#![feature(raw_vec_internals)]
#![feature(const_fn)]
#[cfg(not(all(target_pointer_width = "64", target_arch = "x86_64")))]
compile_error!("Requires x86_64 with 64 bit pointer width.");

extern crate alloc as stdalloc;

extern crate packed_struct;
#[macro_use]
extern crate packed_struct_codegen;

pub mod alloc;
pub mod gc;

use crate::{
    alloc::{BlockHeader, BlockMetadata, GcAllocator, GlobalAllocator},
    gc::{Collector, CollectorPhase},
};
use std::{
    alloc::{Alloc, Layout},
    mem::{forget, transmute, ManuallyDrop},
    ops::{Deref, DerefMut},
};

use parking_lot::Mutex;

#[global_allocator]
static ALLOCATOR: GlobalAllocator = GlobalAllocator;

static mut GC_ALLOCATOR: GcAllocator = GcAllocator;

static COLLECTOR: Mutex<Collector> = Mutex::new(Collector::new());

static COLLECTOR_PHASE: Mutex<CollectorPhase> = Mutex::new(CollectorPhase::Ready);

/// A garbage collected pointer. 'Gc' stands for 'Garbage collected'.
///
/// The type `Gc<T>` provides shared ownership of a value of type `T`,
/// allocted in the heap. `Gc` pointers are `Copyable`, so new pointers to
/// the same value in the heap can be produced trivially. The lifetime of
/// `T` is tracked automatically: it is freed when the application
/// determines that no references to `T` are in scope. This does not happen
/// deterministically, and no guarantees are given about when a value
/// managed by `Gc` is freed.
///
/// Shared references in Rust disallow mutation by default, and `Gc` is no
/// exception: you cannot generally obtain a mutable reference to something
/// inside an `Gc`. If you need mutability, put a `Cell` or `RefCell` inside
/// the `Gc`.
///
/// Unlike `Rc<T>`, cycles between `Gc` pointers are allowed and can be
/// deallocated without issue.
///
/// `Gc<T>` automatically dereferences to `T` (via the `Deref` trait), so
/// you can call `T`'s methods on a value of type `Gc<T>`.
///
/// `Gc<T>` is implemented using a tracing mark-sweep garbage collection
/// algorithm. This means that by using `Gc` pointers in a Rust application,
/// you pull in the overhead of a run-time garbage collector to manage and
/// free `Gc` values behind the scenes.
#[derive(Debug)]
pub struct Gc<T> {
    objptr: *mut T,
}

impl<T> Gc<T> {
    /// Constructs a new `Gc<T>`.
    pub fn new(v: T) -> Self {
        let boxed = GcBox::new(v);
        Gc {
            objptr: boxed as *mut T,
        }
    }

    /// Get a raw pointer to the underlying value `T`.
    pub fn as_ptr(&self) -> *const T {
        self.objptr
    }
}

/// A `GcBox` is a 0-cost wrapper which allows a single `Drop` implementation
/// while also permitting multiple, copyable `Gc` references. The `drop` method
/// on `GcBox` acts as a guard, preventing the destructors on its contents from
/// running unless the object is really dead.
pub(crate) struct GcBox<T>(ManuallyDrop<T>);

impl<T> GcBox<T> {
    fn new(value: T) -> *mut GcBox<T> {
        let layout = Layout::new::<T>();

        let ptr = unsafe { GC_ALLOCATOR.alloc(layout).unwrap().as_ptr() } as *mut GcBox<T>;
        let gcbox = GcBox(ManuallyDrop::new(value));
        unsafe {
            ptr.copy_from_nonoverlapping(&gcbox, 1);
        }

        forget(gcbox);

        unsafe {
            let fatptr: &dyn Drop = &*ptr;
            let vptr = transmute::<*const dyn Drop, (usize, *mut u8)>(fatptr).1;
            (*ptr).set_drop_vptr(vptr);
        }

        ptr
    }

    fn metadata(&self) -> BlockMetadata {
        unsafe {
            let headerptr = (self as *const GcBox<T> as *mut BlockHeader).sub(1);
            (&*headerptr).metadata()
        }
    }

    fn set_metadata(&mut self, header: BlockMetadata) {
        unsafe {
            let headerptr = (self as *const GcBox<T> as *mut BlockHeader).sub(1);
            (*headerptr).set_metadata(header)
        }
    }

    pub(crate) fn set_colour(&mut self, colour: Colour) {
        let mut metadata = self.metadata();
        match colour {
            Colour::Black => metadata.mark_bit = true,
            Colour::White => metadata.mark_bit = false,
        }
        self.set_metadata(metadata);
    }

    pub(crate) fn colour(&self) -> Colour {
        let metadata = self.metadata();
        if metadata.mark_bit {
            Colour::Black
        } else {
            Colour::White
        }
    }

    pub(crate) fn set_dropped(&mut self, value: bool) {
        let mut metadata = self.metadata();
        metadata.dropped = value.into();
        self.set_metadata(metadata);
    }

    pub(crate) fn set_drop_vptr(&mut self, value: *mut u8) {
        let mut metadata = self.metadata();
        metadata.drop_vptr = (value as u64).into();
        self.set_metadata(metadata);
    }

    pub(crate) fn drop_vptr(&self) -> *mut u8 {
        let vptr = *self.metadata().drop_vptr as u64;
        vptr as usize as *mut u8
    }
}

impl<T> Deref for Gc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(self.objptr as *const T) }
    }
}

impl<T> DerefMut for Gc<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *(self.objptr as *mut T) }
    }
}

impl<T> Drop for GcBox<T> {
    fn drop(&mut self) {
        if self.colour() == Colour::Black || self.metadata().dropped {
            return;
        }
        self.set_dropped(true);
        unsafe { ManuallyDrop::drop(&mut self.0) };
    }
}

/// `Copy` and `Clone` are implemented manually because a reference to `Gc<T>`
/// should be copyable regardless of `T`. It differs subtly from `#[derive(Copy,
/// Clone)]` in that the latter only makes `Gc<T>` copyable if `T` is.
impl<T> Copy for Gc<T> {}

impl<T> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Colour of an object used during marking phase (see Dijkstra tri-colour
/// abstraction)
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum Colour {
    Black,
    White,
}

/// Perform a stop-the-world garbage collection. It is not recommended to call
/// this manually and instead let the collector decide when a collection is
/// necessary.
///
/// Calling `collect` when you believe that `Gc` managed values are no longer
/// used is not guaranteed to free those values and should not be relied upon.
pub fn collect() {
    COLLECTOR.lock().collect()
}

pub fn debug_flags(flags: gc::DebugFlags) {
    COLLECTOR.lock().debug_flags = flags;
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
    pub fn is_black<T>(gc: Gc<T>) -> bool {
        assert_eq!(*COLLECTOR_PHASE.lock(), gc::CollectorPhase::Ready);

        let obj = unsafe { &*(gc.objptr as *const GcBox<gc::OpaqueU8>) };
        obj.colour() == Colour::Black
    }

    pub unsafe fn keep_alive<T>(gc: Gc<T>) {
        let obj = &mut *(gc.objptr as *mut GcBox<gc::OpaqueU8>);
        obj.set_colour(Colour::Black)
    }
}
