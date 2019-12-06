use stdalloc::raw_vec::RawVec;

use std::{
    alloc::{Alloc, AllocErr, GlobalAlloc, Layout, System},
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use packed_struct::prelude::*;

/// A spinlock for keeping allocation thread-safe.
///
/// System allocators are generally thread-safe, but gcmalloc reads and writes
/// metadata about each allocation to shared memory upon returning from the
/// system allocator call. This requires additional synchronisation mechanisms.
/// It is not possible to use `sys::Sync::Mutex` for this purpose as the
/// implementation includes a heap allocation containing a `pthread_mutex`.
/// Since `alloc` and friends are not re-entrant, it's not possible to use this.
static ALLOC_LOCK: AllocLock = AllocLock::new();

static mut LAST_ALLOCED: *mut BlockHeader = ::std::ptr::null_mut();

pub struct GlobalAllocator;
pub struct GcAllocator;

unsafe impl GlobalAlloc for GlobalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        alloc(layout, false)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        realloc(ptr, layout, new_size)
    }
}

unsafe impl Alloc for GcAllocator {
    unsafe fn alloc(&mut self, layout: Layout) -> Result<NonNull<u8>, AllocErr> {
        let p = alloc(layout, true);
        Ok(NonNull::new_unchecked(p))
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        dealloc(ptr.as_ptr(), layout);
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>, AllocErr> {
        let newptr = realloc(ptr.as_ptr(), layout, new_size);
        Ok(NonNull::new_unchecked(newptr))
    }
}

#[inline]
unsafe fn alloc(layout: Layout, is_gc: bool) -> *mut u8 {
    ALLOC_LOCK.lock();
    let (l2, uoff) = Layout::new::<BlockHeader>().extend(layout).unwrap();
    let p = System.alloc(l2).add(uoff);
    let headerptr = (p as *mut BlockHeader).sub(1);

    let header = BlockHeader::new(layout.size(), is_gc);
    BlockHeader::patch_next(LAST_ALLOCED, headerptr);
    ::std::ptr::write(headerptr, header);
    LAST_ALLOCED = headerptr;

    ALLOC_LOCK.unlock();
    p
}

#[inline]
unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    ALLOC_LOCK.lock();
    let (l2, uoff) = Layout::new::<BlockHeader>().extend(layout).unwrap();

    let headerptr = (ptr as *mut BlockHeader).sub(1);
    let header = ::std::ptr::read(headerptr);

    BlockHeader::patch_next(header.prev, header.next);
    BlockHeader::patch_prev(header.next, header.prev);

    if headerptr == LAST_ALLOCED {
        LAST_ALLOCED = header.prev
    }
    System.dealloc(ptr.sub(uoff), l2);
    ALLOC_LOCK.unlock();
}

#[inline]
unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOC_LOCK.lock();
    let old_hp = (ptr as *mut BlockHeader).sub(1);
    let header = ::std::ptr::read(old_hp);

    let (l2, uoff) = Layout::new::<BlockHeader>().extend(layout).unwrap();
    let p = System.realloc(ptr.sub(uoff), l2, new_size + uoff).add(uoff);

    let new_hp = (p as *mut BlockHeader).sub(1);
    header.metadata().size = (new_size as u64).into();

    BlockHeader::patch_next(header.prev, new_hp);
    BlockHeader::patch_prev(header.next, new_hp);
    ptr::write(new_hp, header);

    if old_hp == LAST_ALLOCED {
        LAST_ALLOCED = new_hp
    }
    ALLOC_LOCK.unlock();

    p
}

pub struct BlockHeader {
    prev: *mut BlockHeader,
    next: *mut BlockHeader,
    metadata: [u8; SIZE_BLOCKMETADATA],
}

const SIZE_BLOCKMETADATA: usize = 16;

#[cfg(not(all(target_pointer_width = "64", target_arch = "x86_64")))]
compile_error!("Requires x86_64 with 64 bit pointer width.");
#[derive(PackedStruct, Debug)]
#[packed_struct(bit_numbering = "msb0")]
pub struct BlockMetadata {
    /// The size of the block (excluding its header).
    #[packed_field(size_bits = "62", endian = "msb")]
    pub(crate) size: Integer<u64, packed_bits::Bits62>,
    #[packed_field(size_bits = "1")]
    pub(crate) is_gc: bool,
    /// Used by the GC to determine object's reachability.
    #[packed_field(size_bits = "1")]
    pub(crate) mark_bit: bool,
    /// The pointer to the vtable for `Gc<T>`s `Drop` implementation.
    #[packed_field(size_bits = "63", endian = "msb")]
    pub(crate) drop_vptr: Integer<u64, packed_bits::Bits63>,
    /// Has `Drop::drop` been run?
    #[packed_field(size_bits = "1")]
    pub(crate) dropped: bool,
}

impl BlockMetadata {
    fn new(size: usize, is_gc: bool) -> Self {
        Self {
            size: (size as u64).into(),
            is_gc,
            mark_bit: false,
            drop_vptr: (ptr::null_mut::<u8>() as u64).into(),
            dropped: false,
        }
    }
}

impl BlockHeader {
    fn new(size: usize, is_gc: bool) -> Self {
        let metadata = BlockMetadata::new(size, is_gc);

        Self {
            prev: unsafe { LAST_ALLOCED },
            next: ptr::null_mut::<BlockHeader>(),
            metadata: metadata.pack(),
        }
    }

    unsafe fn patch_next(header: *mut BlockHeader, next: *mut BlockHeader) {
        if !header.is_null() {
            let mut h = ptr::read(header);
            h.next = next;
            ptr::write(header, h);
        }
    }

    unsafe fn patch_prev(header: *mut BlockHeader, prev: *mut BlockHeader) {
        if !header.is_null() {
            let mut h = ptr::read(header);
            h.prev = prev;
            ptr::write(header, h);
        }
    }

    pub(crate) fn metadata(&self) -> BlockMetadata {
        BlockMetadata::unpack(&(self.metadata)).unwrap()
    }

    pub(crate) fn set_metadata(&mut self, metadata: BlockMetadata) {
        self.metadata = metadata.pack();
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

impl GlobalAllocator {
    pub(crate) fn iter(&self) -> BlocksIter {
        BlocksIter {
            idx: 0,
            next: unsafe { LAST_ALLOCED },
        }
    }
}

pub(crate) struct BlocksIter {
    idx: usize,
    next: *mut BlockHeader,
}

impl Iterator for BlocksIter {
    type Item = PtrInfo;

    fn next(&mut self) -> Option<PtrInfo> {
        if self.next.is_null() {
            return None;
        }

        let header = unsafe { ptr::read(self.next) };
        let res = Some(PtrInfo {
            ptr: unsafe { (self.next).add(1) } as usize,
            size: *header.metadata().size as usize,
            gc: header.metadata().is_gc,
        });

        self.idx += 1;
        self.next = header.prev;

        res
    }
}

/// A GcVec is a contiguous growable array type whose contents are not tracked
/// for garbage collection.
///
/// The API to `GcVec` can be thought of as a heavily stripped down version of
/// `Vec`. It is intended only to be used internally as part of the Collector
/// implementation where objects do not need keeping alive by the GC.
pub(crate) struct GcVec<T> {
    buf: RawVec<T, System>,
    len: usize,
}

impl<T> GcVec<T> {
    #[inline]
    pub const fn new() -> GcVec<T> {
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
            ::std::ptr::write(end, value);
            self.len += 1;
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            None
        } else {
            unsafe {
                self.len -= 1;
                Some(ptr::read(self.get_unchecked(self.len())))
            }
        }
    }

    /// Reserves capacity for at least `additional` more elements to be inserted
    /// in the given `GcVec<T>`. The collection may reserve more space to avoid
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
}

impl<T> ::std::ops::Deref for GcVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        unsafe {
            let p = self.buf.ptr();
            ::core::intrinsics::assume(!p.is_null());
            ::std::slice::from_raw_parts(p, self.len)
        }
    }
}

impl<T> ::std::ops::DerefMut for GcVec<T> {
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
