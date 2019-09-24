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

use std::{
    alloc::{GlobalAlloc, Layout},
    sync::atomic::{AtomicBool, Ordering},
};

static SIZE_ALLOC_INFO: usize = (1024 * 1024) * 2; // 2KB

static mut ALLOC_INFO: Option<AllocList> = None;

/// A spinlock for the global ALLOC_INFO list.
///
/// System allocators are generally thread-safe, but gcmalloc reads and writes
/// metadata about each allocation to shared memory upon returning from the
/// system allocator call. This requires additional synchronisation mechanisms.
/// It is not possible to use `sys::Sync::Mutex` for this purpose as the
/// implementation includes a heap allocation containing a `pthread_mutex`. This
/// would cause infinite recursion when initialising the lock as the allocator
/// would call itself.
///
/// Instead, we use a simple global spinlock built on an atomic bool to guard
/// access to the ALLOC_INFO list.
static ALLOC_LOCK: AllocLock = AllocLock::new();

pub struct GCMalloc;

unsafe impl GlobalAlloc for GCMalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = libc::malloc(layout.size() as libc::size_t) as *mut u8;

        ALLOC_LOCK.lock();
        match ALLOC_INFO {
            Some(ref mut pm) => pm.insert(ptr as usize, layout.size()),
            None => {
                let mut al = AllocList::new();
                al.insert(ptr as usize, layout.size());
                ALLOC_INFO = Some(al);
            }
        };
        ALLOC_LOCK.unlock();
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        libc::free(ptr as *mut libc::c_void);
        ALLOC_LOCK.lock();
        ALLOC_INFO.as_mut().unwrap().remove(ptr as usize);
        ALLOC_LOCK.unlock();
    }

    unsafe fn realloc(&self, ptr: *mut u8, _layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = libc::realloc(ptr as *mut libc::c_void, new_size) as *mut u8;

        ALLOC_LOCK.lock();
        if ptr.is_null() {
            // realloc is equivalent to malloc if called with a null pointer,
            // thus we must insert the new block info in the ALLOC_INFO list.
            ALLOC_INFO.as_mut().unwrap().insert(ptr as usize, new_size);
        } else {
            // In all other cases, the memory block is resized and a new ptr
            // is returned. This may differ from the original ptr if the new
            // block is too large to fit in the same space as the old block
            ALLOC_INFO
                .as_mut()
                .unwrap()
                .update(ptr as usize, new_ptr as usize, new_size);
        }
        ALLOC_LOCK.unlock();
        new_ptr
    }
}

pub(crate) struct AllocLock(AtomicBool);

impl AllocLock {
    pub(crate) const fn new() -> AllocLock {
        Self(AtomicBool::new(false))
    }

    pub(crate) fn lock(&self) {
        while !self.0.compare_and_swap(false, true, Ordering::AcqRel) {
            // Spin
        }
    }

    pub(crate) fn unlock(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A contiguous chunk of memory which records metadata about each pointer
/// allocated by the allocator. Any pointer can be queried during runtime to
/// determine whether it points directly to, or inside an allocation block on
/// the Rust heap.
///     
/// The size of the allocation block is also recorded, so for each pointer into
/// the heap the exact size of the allocation block can be known, allowing a
/// conservative GC to know which additional machine words must be scanned.
///
/// TODO: Grow the AllocList if it exceeds its size.
pub(crate) struct AllocList {
    start: *const Block,
    next_free: usize,
    can_bump: bool,
}

struct AllocListIter<'a> {
    alloc_list: &'a AllocList,
    idx: usize,
}

struct AllocListIterMut<'a> {
    alloc_list: &'a mut AllocList,
    idx: usize,
}

impl<'a> Iterator for AllocListIterMut<'a> {
    type Item = &'a mut Block;

    fn next(&mut self) -> Option<Self::Item> {
        // It's UB to call `.add` on a pointer past its allocation bounds, so we
        // need to check that it's within range before turning it into a pointer
        // and dereferencing it
        if self.idx * core::mem::size_of::<Block>() >= SIZE_ALLOC_INFO {
            return None;
        }

        let ptr = self.alloc_list.start as usize + (self.idx * core::mem::size_of::<Block>());
        self.idx += 1;

        unsafe { Some(&mut *(ptr as *mut Block)) }
    }
}

impl<'a> Iterator for AllocListIter<'a> {
    type Item = &'a Block;

    fn next(&mut self) -> Option<&'a Block> {
        // It's UB to call `.add` on a pointer past its allocation bounds, so we
        // need to check that it's within range before turning it into a pointer
        if self.idx * core::mem::size_of::<Block>() >= SIZE_ALLOC_INFO {
            return None;
        }

        let ptr = self.alloc_list.start as usize + (self.idx * core::mem::size_of::<Block>());
        self.idx += 1;

        let entry = unsafe { &*(ptr as *const Block) };
        Some(&entry)
    }
}

impl AllocList {
    pub(crate) fn new() -> AllocList {
        let raw = unsafe { libc::malloc(SIZE_ALLOC_INFO as libc::size_t) } as *const Block;
        AllocList {
            start: raw,
            next_free: 0,
            can_bump: true,
        }
    }

    fn iter(&self) -> AllocListIter {
        AllocListIter {
            alloc_list: self,
            idx: 0,
        }
    }

    fn iter_mut(&mut self) -> AllocListIterMut {
        AllocListIterMut {
            alloc_list: self,
            idx: 0,
        }
    }

    /// Performs fast bump pointer insertion until the list is full, at which
    /// point, insertion is O(n) while it linearly scans the list for the next
    /// free entry.
    pub(crate) fn insert(&mut self, ptr: usize, size: usize) {
        debug_assert_ne!(ptr, 0);
        debug_assert_ne!(size, 0);

        if self.can_bump {
            unsafe {
                let next_ptr = self.start.add(self.next_free) as *mut Block;
                if (next_ptr as usize) < self.start as usize + SIZE_ALLOC_INFO {
                    *next_ptr = Block::Entry(core::num::NonZeroUsize::new_unchecked(ptr), size);
                    self.next_free += 1;
                    return;
                } else {
                    self.can_bump = false;
                }
            }
        }

        // Slow path, we need to linearly scan for the next free block in the
        // heap.
        for block in self.iter_mut() {
            if let Block::Free = block {
                *block = Block::Entry(unsafe { core::num::NonZeroUsize::new_unchecked(ptr) }, size);
                return;
            }
        }

        // The allocation list is full
        abort();
    }

    /// Remove ptr information associated with a base pointer (perfomed on a
    /// dealloc). This must not be called with an inner pointer.
    pub(crate) fn remove(&mut self, ptr: usize) {
        for block in self.iter_mut() {
            if let Block::Entry(base_ptr, _) = block {
                if base_ptr.get() == ptr {
                    *block = Block::Free;
                    return;
                }
            }
        }
    }

    /// Given an arbitrary pointer (base or inner), finds pointer info of the
    /// associated base pointer if it exists.
    ///
    /// # Safety
    ///
    /// This method is *not* thread-safe. It is the caller's responsibility to
    /// ensure that no allocation takes place while the `ALLOC_INFO` list is
    /// being read from.
    ///
    /// In conservative GC, this guarantee is implicit as this method is only
    /// ever called during a stop-the-world stack scanning phase where we can be
    /// certain no mutator threads are running.
    pub(crate) fn find_base(&self, ptr: usize) -> Option<PtrInfo> {
        self.iter().find_map(|x| {
            if let Block::Entry(base_ptr, size) = *x {
                if ptr >= base_ptr.get() && ptr < (base_ptr.get() + size as usize) {
                    return Some(PtrInfo {
                        ptr: base_ptr.get(),
                        size,
                    });
                }
            }
            None
        })
    }

    /// Updates the size associated with a base pointer (perfomed on a realloc).
    /// This must not be called with an inner pointer.
    pub(crate) fn update(&mut self, ptr: usize, new_ptr: usize, size: usize) {
        for block in self.iter_mut() {
            if let Block::Entry(base_ptr, _) = *block {
                if ptr == base_ptr.get() {
                    *block = Block::Entry(
                        unsafe { core::num::NonZeroUsize::new_unchecked(new_ptr) },
                        size,
                    );
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
enum Block {
    Free,
    // It is UB to call the raw allocator with a ZST. All instances in the
    // standard library use a Unique::empty() abstraction and ensure that the
    // raw allocator is never called in such instance. Assuming that users
    // adhere to this too, encoding the pointer as a NonZeroUsize means that the
    // value for 0 can be used to encode the discriminant tag. This reduces the
    // size of a `Block` from 3 machine words to 2.
    Entry(core::num::NonZeroUsize, usize),
}

/// The metadata for each ptr is only 2 machine words which means that copying
/// the data out of the `AllocList` is not that expensive. This is done so that
/// the API is cleaner:
///     * We abstract away the complexities of the underlying block structure
///     * It is easier to work with primitives than references to primitives
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct PtrInfo {
    pub ptr: usize,
    pub size: usize,
}

fn abort() {
    unsafe { core::intrinsics::abort() };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_find_ptr() {
        let mut al = AllocList::new();
        let pm = PtrInfo { ptr: 1234, size: 4 };
        al.insert(pm.ptr, pm.size);

        assert_eq!(al.find_base(pm.ptr).unwrap(), pm)
    }

    #[test]
    fn find_inner_ptr() {
        let mut al = AllocList::new();
        let pm = PtrInfo { ptr: 1234, size: 4 };

        al.insert(pm.ptr, pm.size);

        assert_eq!(al.find_base(1235).unwrap(), pm);
        assert_eq!(al.find_base(1236).unwrap(), pm);
        assert_eq!(al.find_base(1237).unwrap(), pm);
        assert!(al.find_base(1238).is_none());
    }

    #[test]
    fn free_block() {
        let mut al = AllocList::new();
        let pm = PtrInfo { ptr: 1234, size: 4 };
        al.insert(pm.ptr, pm.size);

        al.remove(pm.ptr);

        assert!(al.find_base(1234).is_none());
    }

    #[test]
    fn can_alloc_a_freed_block() {
        let mut al = AllocList::new();

        let num_ptrs = SIZE_ALLOC_INFO / core::mem::size_of::<Block>();
        for i in 0..num_ptrs {
            al.insert(i + 1, 1);
        }

        // // Free the pointer in the middle of the list
        al.remove(num_ptrs / 2);
        let pm = PtrInfo { ptr: 1234, size: 1 };
        al.insert(pm.ptr, pm.size);

        assert_eq!(al.find_base(pm.ptr).unwrap(), pm);
    }
}
