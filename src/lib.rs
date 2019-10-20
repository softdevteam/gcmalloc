// Copyright (c) 2019 King's College London
// Created by the Software Development Team <http://soft-dev.org/>
//
// The Universal Permissive License (UPL), Version 1.0
//
// Subject to the condition set forth below, permission is hereby granted to any
// person obtaining a copy of this software, associated documentation and/or
// data (collectively the "Software"), free of charge and under any and all
// copyright rights in the Software, and any and all patent rights owned or
// freely licensable by each licensor hereunder covering either (i) the
// unmodified Software as contributed to or provided by such licensor, or (ii)
// the Larger Works (as defined below), to deal in both
//
// (a) the Software, and
// (b) any piece of software and/or hardware listed in the lrgrwrks.txt file
// if one is included with the Software (each a "Larger Work" to which the Software
// is contributed by such licensors),
//
// without restriction, including without limitation the rights to copy, create
// derivative works of, display, perform, and distribute the Software and make,
// use, sell, offer for sale, import, export, have made, and have sold the
// Software and the Larger Work(s), and to sublicense the foregoing rights on
// either these or other terms.
//
// This license is subject to the following condition: The above copyright
// notice and either this complete permission notice or at a minimum a reference
// to the UPL must be included in all copies or substantial portions of the
// Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

#![crate_name = "gcmalloc"]
#![crate_type = "rlib"]
#![feature(core_intrinsics)]
#![feature(allocator_api)]
#![feature(alloc_layout_extra)]

mod alloc;

use crate::alloc::{AllocMetadata, AllocWithInfo, GCMalloc};
use std::{
    alloc::{Alloc, Layout},
    mem::{forget, size_of},
    ptr,
};

#[global_allocator]
static ALLOCATOR: AllocWithInfo = AllocWithInfo;

/// Used for allocation of objects which are managed by the collector (through
/// the `Gc` smart pointer interface).
static mut GC_ALLOCATOR: GCMalloc = GCMalloc;

struct Gc<T> {
    objptr: *mut T,
}

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
            ptr::write(headerptr as *mut usize, 1);
            objptr as *mut T
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_with_gc() {
        let gc = Gc::new(1234);
        let pi = AllocMetadata::find(gc.objptr as usize).unwrap();
        assert!(pi.gc)
    }
}
