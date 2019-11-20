#![crate_name = "gcmalloc"]
#![crate_type = "rlib"]
#![feature(core_intrinsics)]
#![feature(allocator_api)]
#![feature(alloc_layout_extra)]

#[cfg(not(all(target_pointer_width = "64", target_arch = "x86_64")))]
compile_error!("Requires x86_64 with 64 bit pointer width.");

pub mod alloc;
pub mod gc;

use crate::{
    alloc::{AllocMetadata, AllocWithInfo, GCMalloc},
    gc::Collector,
};
use std::{
    alloc::{Alloc, Layout},
    cell::Cell,
    mem::{forget, size_of},
    ops::{Deref, DerefMut},
    ptr,
};

#[global_allocator]
static ALLOCATOR: AllocWithInfo = AllocWithInfo;

/// Used for allocation of objects which are managed by the collector (through
/// the `Gc` smart pointer interface).
static mut GC_ALLOCATOR: GCMalloc = GCMalloc;

static mut COLLECTOR: Option<Collector> = None;

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

/// A garbage collected value is stored with a 1 machine word sized header. This
/// header stores important metadata used by the GC during collection. It should
/// never be accessible to users of the GC library.
///
/// The metadata inside the header contains the mark-bit used by the collector
/// to determine the value's reachability. It also contains the offset from the
/// `objptr` to the `baseptr` in machine words. For example, if this value was
/// 8, then the `baseptr` is, on a 64-bit machine, 64 bytes before the `objptr`.
/// This is required in instances where a greater-than-usize alignment is used
/// to store `T`, as `dealloc` requires the allocation block's `baseptr`, *not*
/// the `objptr`. This will only work on alignments small enough to fit in 63
/// bits. The bitpattern of this is as follows:
///
/// 0:       mark-bit
/// 1..63:   offset in words to baseptr
#[derive(Debug)]
struct GcHeader(Cell<usize>);

impl<T> Gc<T> {
    /// Constructs a new `Gc<T>`.
    pub fn new(v: T) -> Self {
        let objptr = Self::alloc_blank(Layout::new::<T>());
        let gc = unsafe {
            objptr.copy_from_nonoverlapping(&v, 1);
            Gc::from_raw(objptr)
        };
        forget(v);
        gc
    }

    /// Create a `Gc` from a raw pointer previously created by `alloc_blank` or
    /// `into_raw`.
    pub unsafe fn from_raw(objptr: *const T) -> Self {
        Gc {
            objptr: objptr as *mut T,
        }
    }

    /// Get a raw pointer to the underlying value `T`.
    pub fn as_ptr(&self) -> *const T {
        self.objptr
    }

    /// Returns a reference to the header of the `Gc<T>`. The header stores
    /// meta-data about the value used by the collector. For example, the
    /// mark-bit used to denote the object's reachability, is stored in
    /// the header.
    fn header(&self) -> &GcHeader {
        unsafe {
            let hoff = (self.objptr as *const i8).sub(size_of::<GcHeader>());
            &*(hoff as *const GcHeader)
        }
    }

    pub(crate) fn base_ptr_offset(&self) -> usize {
        self.header().0.get() & !1
    }

    /// Get the value of the mark bit stored in the header of the `Gc<T>` value.
    pub(crate) fn mark_bit(&self) -> bool {
        self.header().mark_bit()
    }

    pub(crate) fn set_mark_bit(&self, value: bool) {
        self.header().set_mark_bit(value)
    }

    /// Allocate memory sufficient to `l` (i.e. correctly aligned and of at
    /// least the required size). The returned pointer must be passed to
    /// `Gc::from_raw`.
    pub fn alloc_blank(l: Layout) -> *mut T {
        let (layout, uoff) = Layout::new::<usize>().extend(l).unwrap();
        // In order for our storage scheme to work, it's necessary that `uoff -
        // sizeof::<usize>()` gives a valid alignment for a `usize`. There are
        // only two cases we need to consider here:
        //   1) `object`'s alignment is smaller than or equal to `usize`. If so,
        //      no padding will be added, at which point by definition `uoff -
        //      sizeof::<usize>()` will be exactly equivalent to the start point
        //      of the layout.
        //   2) `object`'s alignment is bigger than `usize`. Since alignment
        //      must be a power of two, that means that we must by definition be
        //      adding at least one exact multiple of `usize` bytes of padding.
        unsafe {
            let baseptr = GC_ALLOCATOR.alloc(layout).unwrap().as_ptr();
            let objptr = baseptr.add(uoff);

            // size excl. header and padding
            let objsize = layout.size() - (objptr as usize - baseptr as usize);
            AllocMetadata::insert(objptr as usize, objsize, true);

            let headerptr = objptr.sub(size_of::<usize>());
            ptr::write(headerptr as *mut GcHeader, GcHeader::new(uoff));
            objptr as *mut T
        }
    }
}

impl GcHeader {
    pub(crate) fn new(uoff: usize) -> Self {
        let white = unsafe { !COLLECTOR.as_ref().unwrap().current_black() };
        let header = Self(Cell::new(uoff));
        header.set_mark_bit(white);
        header
    }

    pub(crate) fn mark_bit(&self) -> bool {
        (self.0.get() & 1) == 1
    }

    pub(crate) fn set_mark_bit(&self, mark: bool) {
        if mark {
            self.0.set(self.0.get() | 1);
        } else {
            self.0.set(self.0.get() & !1);
        }
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

/// `Copy` and `Clone` are implemented manually because a reference to `Gc<T>`
/// should be copyable regardless of `T`. It differs subtly from `#[derive(Copy,
/// Clone)]` in that the latter only makes `Gc<T>` copyable if `T` is.
impl<T> Copy for Gc<T> {}

impl<T> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Initialize the garbage collector. This *must* happen before any `Gc<T>`s are
/// allocated.
pub fn init(flags: gc::DebugFlags) {
    unsafe { COLLECTOR = Some(Collector::new(flags)) };
}

/// Perform a stop-the-world garbage collection. It is not recommended to call
/// this manually and instead let the collector decide when a collection is
/// necessary.
///
/// Calling `collect` when you believe that `Gc` managed values are no longer
/// used is not guaranteed to free those values and should not be relied upon.
pub fn collect() {
    unsafe { COLLECTOR.as_mut().unwrap().collect() }
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
        let collector = unsafe { COLLECTOR.as_ref().unwrap() };
        let cstate = *collector.state.lock().unwrap();

        // Checking an object's colour only makes sense immediately after
        // marking has taken place. After a full collection has happened,
        // marking results are stale and the object graph must be re-marked in
        // order for this query to be meaningful.
        assert!(!collector.debug_flags.sweep_phase);
        assert_eq!(cstate, gc::CollectorState::Ready);

        return collector.colour(unsafe { Gc::from_raw(gc.objptr as *const i8) })
            == gc::Colour::Black;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_with_gc() {
        init(gc::DebugFlags::new());
        let gc = Gc::new(1234);
        let pi = AllocMetadata::find(gc.objptr as usize).unwrap();
        assert!(pi.gc)
    }
}
