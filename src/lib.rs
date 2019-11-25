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

use packed_struct::prelude::*;

pub mod alloc;
pub mod gc;

use crate::{
    alloc::{AllocMetadata, GlobalAllocator},
    gc::Collector,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    mem::{forget, transmute, ManuallyDrop},
    ops::{Deref, DerefMut},
    ptr,
};

use packed_struct::PackedStruct;

#[global_allocator]
static ALLOCATOR: GlobalAllocator = GlobalAllocator;

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
pub(crate) struct GcBox<T>(T);

impl<T> GcBox<T> {
    fn new(value: T) -> *mut GcBox<T> {
        let gcb = GcBox(value);

        let ptr = GcBox::alloc_blank(Layout::new::<T>());
        unsafe {
            ptr.copy_from_nonoverlapping(&gcb, 1);
        }

        let fatptr: &dyn Drop = &gcb;
        unsafe {
            let vptr = transmute::<*const dyn Drop, (usize, usize)>(fatptr).1;
            (&mut *ptr).set_header(GcHeader::new(vptr));
        }

        forget(gcb);

        ptr
    }

    /// Allocate memory sufficient to `l` (i.e. correctly aligned and of at
    /// least the required size). The returned pointer must be passed to
    /// `GcBox::from_raw`.
    pub fn alloc_blank(l: Layout) -> *mut GcBox<T> {
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
            let baseptr = System.alloc(layout);
            let objptr = baseptr.add(uoff);

            // size excl. header and padding
            let objsize = layout.size() - (objptr as usize - baseptr as usize);
            AllocMetadata::insert(objptr as usize, objsize, true);
            objptr as *mut GcBox<T>
        }
    }

    fn header(&self) -> GcHeader {
        unsafe {
            let headerptr = (self as *const GcBox<T> as *const [u8; 8]).sub(1);
            GcHeader::unpack(&*headerptr).unwrap()
        }
    }

    fn set_header(&mut self, header: GcHeader) {
        unsafe {
            let headerptr = (self as *mut GcBox<T> as *mut [u8; 8]).sub(1);
            ptr::write(headerptr, GcHeader::pack(&header));
        }
    }

    pub(crate) fn set_mark_bit(&mut self, value: bool) {
        let mut header = self.header();
        header.mark_bit = value.into();
        self.set_header(header);
    }

    pub(crate) fn drop_vptr(&self) -> *mut u8 {
        let vptr = *self.header().drop_vptr as u64;
        vptr as usize as *mut u8
    }
}

/// A garbage collected value is stored with a 1 machine word sized header. This
/// header stores important metadata used by the GC during collection. It should
/// never be accessible to users of the GC library.
#[derive(PackedStruct, Debug)]
#[packed_struct(bit_numbering = "msb0", size_bytes = "8")]
pub struct GcHeader {
    /// The pointer to the vtable for `Gc<T>`s `Drop` implementation.
    #[packed_field(bits = "0..=61", endian = "msb")]
    drop_vptr: Integer<u64, packed_bits::Bits62>,
    /// Used by the GC during the marking phase
    #[packed_field(bits = "63")]
    mark_bit: bool,
}

impl GcHeader {
    pub(crate) fn new(vptr: usize) -> Self {
        let white = unsafe { !COLLECTOR.as_ref().unwrap().current_black() };
        GcHeader {
            drop_vptr: (vptr as u64).into(),
            mark_bit: white,
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

impl<T> Drop for GcBox<T> {
    fn drop(&mut self) {}
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

        unsafe {
            return collector.colour(&*(gc.objptr as *const GcBox<gc::OpaqueU8>))
                == gc::Colour::Black;
        }
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
