// Copyright (c) 2019 King's College London created by the Software Development
// Team <http://soft-dev.org/>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0>, or the MIT license <LICENSE-MIT
// or http://opensource.org/licenses/MIT>, or the UPL-1.0 license
// <http://opensource.org/licenses/UPL> at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::{
    alloc::{Alloc, AllocErr, GlobalAlloc, Layout},
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

static SIZE_ALLOC_INFO: usize = (1024 * 1024) * 2; // 2MiB

static mut ALLOC_INFO: Option<AllocList> = None;

/// A spinlock for the global ALLOC_INFO list.
///
/// System allocators are generally thread-safe, but gcmalloc reads and writes
/// metadata about each allocation to shared memory upon returning from the
/// system allocator call. This requires additional synchronisation mechanisms.
/// It is not possible to use `sys::Sync::Mutex` for this purpose as the
/// implementation includes a heap allocation containing a `pthread_mutex`.
/// Since `alloc` and friends are not re-entrant, it's not possible to use this.
///
/// Instead, we use a simple global spinlock built on an atomic bool to guard
/// access to the ALLOC_INFO list.
static ALLOC_LOCK: AllocLock = AllocLock::new();

pub struct AllocWithInfo;

unsafe impl GlobalAlloc for AllocWithInfo {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = libc::malloc(layout.size() as libc::size_t) as *mut u8;
        AllocMetadata::insert(ptr as usize, layout.size(), false);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        libc::free(ptr as *mut libc::c_void);
        AllocMetadata::remove(ptr as usize);
    }

    unsafe fn realloc(&self, ptr: *mut u8, _layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = libc::realloc(ptr as *mut libc::c_void, new_size) as *mut u8;

        assert!(!ptr.is_null());
        assert!(new_size > 0);

        AllocMetadata::update(ptr as usize, new_ptr as usize, new_size);
        new_ptr
    }
}

pub struct GCMalloc;

unsafe impl Alloc for GCMalloc {
    unsafe fn alloc(&mut self, layout: Layout) -> Result<NonNull<u8>, AllocErr> {
        let ptr = libc::malloc(layout.size() as libc::size_t) as *mut u8;
        Ok(NonNull::new_unchecked(ptr))
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, _layout: Layout) {
        libc::free(ptr.as_ptr() as *mut libc::c_void);
        AllocMetadata::remove(ptr.as_ptr() as usize);
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        _layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>, AllocErr> {
        let new_ptr = libc::realloc(ptr.as_ptr() as *mut libc::c_void, new_size) as *mut u8;

        assert!(!ptr.as_ptr().is_null());
        assert!(new_size > 0);

        AllocMetadata::update(ptr.as_ptr() as usize, new_ptr as usize, new_size);
        Ok(NonNull::new_unchecked(new_ptr))
    }
}

struct AllocLock(AtomicBool);

impl AllocLock {
    const fn new() -> AllocLock {
        Self(AtomicBool::new(false))
    }

    fn lock(&self) {
        while self.0.compare_and_swap(false, true, Ordering::AcqRel) {
            // Spin
        }
    }

    fn unlock(&self) {
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
struct AllocList {
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
        if self.idx * size_of::<Block>() >= (SIZE_ALLOC_INFO - size_of::<Block>()) {
            return None;
        }

        // If we're still doing bump insertion, then there's a chunk of the list
        // which remains uninitialized.
        if self.alloc_list.can_bump && self.idx >= self.alloc_list.next_free {
            return None;
        }

        let ptr = self.alloc_list.start as usize + (self.idx * size_of::<Block>());
        self.idx += 1;

        unsafe { Some(&mut *(ptr as *mut Block)) }
    }
}

impl<'a> Iterator for AllocListIter<'a> {
    type Item = &'a Block;

    fn next(&mut self) -> Option<&'a Block> {
        // It's UB to call `.add` on a pointer past its allocation bounds, so we
        // need to check that it's within range before turning it into a pointer
        if self.idx * size_of::<Block>() >= SIZE_ALLOC_INFO {
            return None;
        }

        // If we're still doing bump insertion, then there's a chunk of the list
        // which remains uninitialized.
        if self.alloc_list.can_bump && self.idx >= self.alloc_list.next_free {
            return None;
        }

        let ptr = self.alloc_list.start as usize + (self.idx * size_of::<Block>());
        self.idx += 1;

        let entry = unsafe { &*(ptr as *const Block) };
        Some(&entry)
    }
}

impl AllocList {
    fn new() -> AllocList {
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
    fn insert(&mut self, ptr: usize, size: usize, gc: bool) {
        debug_assert_ne!(ptr, 0);
        debug_assert_ne!(size, 0);

        if self.can_bump {
            unsafe {
                let next_ptr = self.start.add(self.next_free) as *mut Block;
                let last = self.start as usize + SIZE_ALLOC_INFO - size_of::<Block>();
                if (next_ptr as usize) <= last {
                    *next_ptr = Block::Entry(core::num::NonZeroUsize::new_unchecked(ptr), size, gc);
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
                *block = Block::Entry(
                    unsafe { core::num::NonZeroUsize::new_unchecked(ptr) },
                    size,
                    gc,
                );
                return;
            }
        }

        // The allocation list is full
        exit_alloc("Allocation failed: Metadata list full\n");
    }

    /// Remove ptr information associated with a base pointer (perfomed on a
    /// dealloc). This must not be called with an inner pointer.
    fn remove(&mut self, ptr: usize) {
        for block in self.iter_mut() {
            if let Block::Entry(base_ptr, ..) = block {
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
    fn find_base(&self, ptr: usize) -> Option<PtrInfo> {
        self.iter().find_map(|x| {
            if let Block::Entry(base_ptr, size, gc) = *x {
                if ptr >= base_ptr.get() && ptr < (base_ptr.get() + size as usize) {
                    return Some(PtrInfo {
                        ptr: base_ptr.get(),
                        size,
                        gc,
                    });
                }
            }
            None
        })
    }

    /// Updates the size associated with a base pointer (perfomed on a realloc).
    /// This must not be called with an inner pointer.
    fn update(&mut self, ptr: usize, new_ptr: usize, size: usize) {
        for block in self.iter_mut() {
            if let Block::Entry(base_ptr, _size, gc) = *block {
                if ptr == base_ptr.get() {
                    *block = Block::Entry(
                        unsafe { core::num::NonZeroUsize::new_unchecked(new_ptr) },
                        size,
                        gc,
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
    Entry(core::num::NonZeroUsize, usize, bool),
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
    pub gc: bool,
}

fn exit_alloc(msg: &str) {
    unsafe { libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len()) };
    std::process::abort();
}

// -----------------------------------------------------------------------------
//        APIs for querying the block information about each allocation
// -----------------------------------------------------------------------------

pub struct AllocMetadata;

impl AllocMetadata {
    /// Insert metadata about an allocation block. The GC scans for reachable
    /// objects conservatively. It uses the metadata stored against each
    /// allocation to determine:
    /// 1) the size of an allocation block
    /// 2) whether an allocation block's memory management is the responsibility
    ///    of the GC
    /// 3) whether an arbitrary pointer-like word points within an
    ///    allocation block
    pub(crate) fn insert(ptr: usize, size: usize, gc: bool) {
        ALLOC_LOCK.lock();
        unsafe {
            match ALLOC_INFO {
                Some(ref mut pm) => pm.insert(ptr as usize, size, gc),
                None => {
                    let mut al = AllocList::new();
                    al.insert(ptr as usize, size, gc);
                    ALLOC_INFO = Some(al);
                }
            };
        }
        ALLOC_LOCK.unlock();
    }
    /// Returns metadata about an allocation block when given an arbitrary
    /// pointer to the start of the block or an offset within it.
    pub(crate) fn find(ptr: usize) -> Option<PtrInfo> {
        ALLOC_LOCK.lock();
        let ptr_info = unsafe { ALLOC_INFO.as_ref().unwrap().find_base(ptr) };
        ALLOC_LOCK.unlock();
        ptr_info
    }

    /// Updates the metadata associated with an allocation so that the block
    /// once pointed to by `ptr` is recorded as having moved to `new_ptr` and is
    /// now `size` bytes big.
    pub(crate) fn update(ptr: usize, new_ptr: usize, size: usize) {
        ALLOC_LOCK.lock();
        unsafe { ALLOC_INFO.as_mut().unwrap().update(ptr, new_ptr, size) };
        ALLOC_LOCK.unlock();
    }

    /// Removes the metadata associated with an allocation.
    pub(crate) fn remove(ptr: usize) {
        ALLOC_LOCK.lock();
        unsafe { ALLOC_INFO.as_mut().unwrap().remove(ptr) };
        ALLOC_LOCK.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static mut NEXT_PTR: usize = 1;
    static mut PTR_LOCK: AllocLock = AllocLock::new();

    fn unique_ptr(size: usize) -> usize {
        unsafe {
            PTR_LOCK.lock();
            let ptr = NEXT_PTR;
            NEXT_PTR += size + 1;
            PTR_LOCK.unlock();
            ptr
        }
    }

    #[test]
    fn insert_and_find_ptr() {
        let size = 4;
        let pi = PtrInfo {
            ptr: unique_ptr(size),
            size,
            gc: false,
        };

        AllocMetadata::insert(pi.ptr, pi.size, pi.gc);
        assert_eq!(AllocMetadata::find(pi.ptr).unwrap(), pi)
    }

    #[test]
    fn find_inner_ptr() {
        let size = 2;
        let pi = PtrInfo {
            ptr: unique_ptr(size),
            size,
            gc: false,
        };
        AllocMetadata::insert(pi.ptr, pi.size, pi.gc);

        for i in 0..size {
            assert_eq!(AllocMetadata::find(pi.ptr + i).unwrap(), pi);
        }

        // Check for off-by-one
        match AllocMetadata::find(pi.ptr + size + 1) {
            Some(pi_actual) => assert_ne!(pi_actual, pi),
            None => (),
        }
    }

    #[test]
    fn free_block() {
        let size = 2;
        let pi = PtrInfo {
            ptr: unique_ptr(size),
            size,
            gc: false,
        };

        AllocMetadata::insert(pi.ptr, pi.size, pi.gc);
        assert!(AllocMetadata::find(pi.ptr).is_some());
        AllocMetadata::remove(pi.ptr);

        assert!(AllocMetadata::find(pi.ptr).is_none());
    }

    #[test]
    fn can_alloc_a_freed_block() {
        let mut al = AllocList::new();

        let num_ptrs = SIZE_ALLOC_INFO / core::mem::size_of::<Block>();
        for i in 0..num_ptrs {
            al.insert(i + 1, 1, false);
        }

        // // Free the pointer in the middle of the list
        al.remove(num_ptrs / 2);
        let pm = PtrInfo {
            ptr: 1234,
            size: 1,
            gc: false,
        };
        al.insert(pm.ptr, pm.size, pm.gc);

        assert_eq!(al.find_base(pm.ptr).unwrap(), pm);
    }

    #[test]
    fn record_gc_alloc() {
        let size = 2;
        let pi = PtrInfo {
            ptr: unique_ptr(size),
            size,
            gc: true,
        };

        AllocMetadata::insert(pi.ptr, pi.size, pi.gc);

        assert!(AllocMetadata::find(pi.ptr).unwrap().gc);
    }
}
