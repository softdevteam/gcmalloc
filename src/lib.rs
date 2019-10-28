// Copyright (c) 2019 King's College London created by the Software Development
// Team <http://soft-dev.org/>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0>, or the MIT license <LICENSE-MIT
// or http://opensource.org/licenses/MIT>, or the UPL-1.0 license
// <http://opensource.org/licenses/UPL> at your option. This file may not be
// copied, modified, or distributed except according to those terms.

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

#[derive(Clone, Copy)]
pub struct Gc<T> {
    objptr: *mut T,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GcHeader(usize);

impl<T> Gc<T> {
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

    pub fn as_ptr(&self) -> *const T {
        self.objptr
    }

    pub(crate) fn header(&self) -> &GcHeader {
        unsafe {
            let hoff = (self.objptr as *const i8).sub(size_of::<GcHeader>());
            &*(hoff as *const GcHeader)
        }
    }

    pub(crate) fn header_mut(&self) -> &mut GcHeader {
        unsafe {
            let hoff = (self.objptr as *const i8).sub(size_of::<GcHeader>());
            &mut *(hoff as *mut GcHeader)
        }
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

            AllocMetadata::insert(objptr as usize, layout.size(), true);
            let headerptr = objptr.sub(size_of::<usize>());
            ptr::write(headerptr as *mut GcHeader, GcHeader::new());
            objptr as *mut T
        }
    }
}

impl GcHeader {
    pub(crate) fn new() -> Self {
        let white = unsafe { !COLLECTOR.as_ref().unwrap().current_black() };
        let mut header = Self(0);
        header.set_mark_bit(white);
        header
    }

    pub(crate) fn mark_bit(&self) -> bool {
        (self.0 & 1) == 1
    }

    pub(crate) fn set_mark_bit(&mut self, mark: bool) {
        if mark {
            self.0 |= 1
        } else {
            self.0 &= !1
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

pub fn init(flags: gc::DebugFlags) {
    unsafe { COLLECTOR = Some(Collector::new(flags)) };
}

pub fn collect() {
    unsafe { COLLECTOR.as_mut().unwrap().collect() }
}

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
        assert_eq!(cstate, gc::CollectorState::FinishedMarking);
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
