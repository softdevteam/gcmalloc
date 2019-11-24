use stdalloc::raw_vec::RawVec;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, Ordering},
};

/// Used by the global allocator to store metadata about every allocated block.
///
/// This information is vital for the collector to know things such as: whether
/// an arbitrary word in a block is a ptr; how to get to the beginning of a
/// block from some inner ptr; and whether a block is managed by the collector.
pub(crate) static mut BLOCK_METADATA: VOH<PtrInfo> = VOH::new();

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

pub struct GlobalAllocator;

unsafe impl GlobalAlloc for GlobalAllocator {
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

/// A VOH (Vector of Holes) is a contiguous growable array type optimised for
/// fast insertion and O(1) removal.
///
/// Unlike the `Vec<T>` type available in the standard library, `VOH<T>`
/// does not shift remaining elements to the left after removing an item.
/// Instead, a `None` value is swapped in its place, effectively leaving a hole
/// inside the vector.
///
/// Accessing elements in a `VOH<T>`, however, can be slower than a
/// `Vec<T>`, as elements are no longer strictly contiguous; fragmentation will
/// occur over time as removed elements increase the distance between those
/// which remain.
///
/// The API to `VOH<T>` can be thought of as a heavily stripped down
/// version of `Vec<T>`. Its use-case is highly specific, and intended only to
/// be used internally as part of the Collector implementation. For this reason,
/// its allocation is not tracked by the collector. Do not store any values
/// inside which you intend the GC to keep alive.
pub(crate) struct VOH<T> {
    buf: RawVec<Option<T>, System>,
    len: usize,
}

impl<T> VOH<T> {
    #[inline]
    pub const fn new() -> VOH<T> {
        Self {
            // use the system allocator instead of the global allocator
            buf: RawVec::new_in(System),
            len: 0,
        }
    }

    /// Appends an element to the back of a collection.
    ///
    /// # Panics
    ///
    /// Panics if the number of elements in the vector overflows a `usize`.
    pub fn push(&mut self, value: T) {
        // This will panic or abort if we would allocate > isize::MAX bytes
        // or if the length increment would overflow for zero-sized types.
        if self.len == self.buf.capacity() {
            self.reserve(1);
        }
        unsafe {
            let end = self.as_mut_ptr().add(self.len);
            ::std::ptr::write(end, Some(value));
            self.len += 1;
        }
    }

    /// Reserves capacity for at least `additional` more elements to be inserted
    /// in the given `VOH<T>`. The collection may reserve more space to avoid
    /// frequent reallocations. After calling `reserve`, capacity will be
    /// greater than or equal to `self.len() + additional`. Does nothing if
    /// capacity is already sufficient.
    ///
    /// # Panics
    ///
    /// Panics if the new capacity overflows `usize`.
    pub fn reserve(&mut self, additional: usize) {
        self.buf.reserve(self.len, additional);
    }

    /// Removes and returns the element at position `index` within the vector if
    /// it exists. Removal is O(1): the value at position `index` is simply
    /// replaced with `None`. Since the vector does not shift all elements after
    /// it to the left, removal does not decrease the length of the vector.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of bounds.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        assert!(index < self.len());
        unsafe {
            // the place we are taking from.
            let ptr = self.as_mut_ptr().add(index);
            // copy it out, unsafely having a copy of the value on
            // the stack and in the vector at the same time.
            let ret = ::std::ptr::read(ptr);

            ::std::ptr::write(ptr, None);
            ret
        }
    }
}

impl<T> ::std::ops::Deref for VOH<T> {
    type Target = [Option<T>];

    fn deref(&self) -> &Self::Target {
        unsafe {
            let p = self.buf.ptr();
            ::core::intrinsics::assume(!p.is_null());
            ::std::slice::from_raw_parts(p, self.len)
        }
    }
}

impl<T> ::std::ops::DerefMut for VOH<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let p = self.buf.ptr();
            ::core::intrinsics::assume(!p.is_null());
            ::std::slice::from_raw_parts_mut(p, self.len)
        }
    }
}

/// Information about an allocation block used by the collector.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct PtrInfo {
    /// The pointer to the beginning of the allocation block.
    pub ptr: usize,
    /// The size of the allocation block in bytes.
    pub size: usize,
    /// Whether the allocation block is managed by the GC or Rust's RAII.
    pub gc: bool,
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
        unsafe { BLOCK_METADATA.push(PtrInfo { ptr, size, gc }) };
        ALLOC_LOCK.unlock();
    }
    /// Returns metadata about an allocation block when given an arbitrary
    /// pointer to the start of the block or an offset within it.
    pub(crate) fn find(ptr: usize) -> Option<PtrInfo> {
        ALLOC_LOCK.lock();
        let block = unsafe {
            BLOCK_METADATA
                .iter()
                .filter_map(|x| *x)
                .find(|x| ptr >= x.ptr && ptr < x.ptr + x.size)
        };
        ALLOC_LOCK.unlock();
        block
    }

    /// Updates the metadata associated with an allocation so that the block
    /// once pointed to by `ptr` is recorded as having moved to `new_ptr` and is
    /// now `size` bytes big.
    pub(crate) fn update(ptr: usize, new_ptr: usize, size: usize) {
        ALLOC_LOCK.lock();
        unsafe {
            let idx = BLOCK_METADATA
                .iter()
                .position(|x| x.map_or(false, |x| x.ptr == ptr))
                .unwrap();

            BLOCK_METADATA[idx] = Some(PtrInfo {
                ptr: new_ptr,
                size,
                gc: BLOCK_METADATA[idx].unwrap().gc,
            });
        }
        ALLOC_LOCK.unlock();
    }

    /// Removes the metadata associated with an allocation.
    pub(crate) fn remove(ptr: usize) {
        ALLOC_LOCK.lock();
        unsafe {
            let idx = BLOCK_METADATA
                .iter()
                .position(|x| x.map_or(false, |x| x.ptr == ptr))
                .unwrap();
            BLOCK_METADATA.remove(idx);
        }
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
