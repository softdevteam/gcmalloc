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


static SIZE_ALLOC_INFO: usize = (1024 * 1024) * 2; // 2KB

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
    fn new() -> AllocList {
        let raw = unsafe { libc::malloc(SIZE_ALLOC_INFO as libc::size_t) } as *mut Block;
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
    fn insert(&mut self, ptr: usize, size: usize) {
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
    fn remove(&mut self, ptr: usize) {
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
    fn find_base(&self, ptr: usize) -> Option<PtrInfo> {
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
    fn update(&mut self, ptr: usize, size: usize) {
        for block in self.iter_mut() {
            if let Block::Entry(base_ptr, _) = *block {
                if ptr == base_ptr.get() {
                    *block = Block::Entry(base_ptr, size);
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
struct PtrInfo {
    ptr: usize,
    size: usize,
}

static mut PTR_METADATA: Option<AllocList> = None;

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
            al.insert(i, 1);
        }

        // // Free the pointer in the middle of the list
        al.remove(num_ptrs / 2);
        let pm = PtrInfo { ptr: 1234, size: 1 };
        al.insert(pm.ptr, pm.size);

        assert_eq!(al.find_base(pm.ptr).unwrap(), pm);
    }
}
