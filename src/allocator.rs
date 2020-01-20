use stdalloc::raw_vec::RawVec;

use std::{
    alloc::{Alloc, AllocErr, GlobalAlloc, Layout, System},
    marker::PhantomData,
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{collector::MarkingCtxt, gc::Colour, ALLOCATOR, COLLECTOR};

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

pub static mut HEAP_TOP: usize = 0;

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
        let mut c = COLLECTOR.lock();
        c.poll();
        c.allocations += 1;
        drop(c); // unlock the collector

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

    let top = p.add(layout.size());
    if top as usize > HEAP_TOP {
        HEAP_TOP = top as usize;
    }

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
    let mut header = ::std::ptr::read(old_hp);

    let (l2, uoff) = Layout::new::<BlockHeader>().extend(layout).unwrap();
    let p = System.realloc(ptr.sub(uoff), l2, new_size + uoff).add(uoff);

    let new_hp = (p as *mut BlockHeader).sub(1);
    let mut md = header.metadata();

    assert_eq_size!(usize, u64);
    md.size = (new_size as u64).into();
    header.set_metadata(md);

    BlockHeader::patch_next(header.prev, new_hp);
    BlockHeader::patch_prev(header.next, new_hp);
    ptr::write(new_hp, header);

    // Max heap boundary
    let top = p.add(layout.size());
    if top as usize > HEAP_TOP {
        HEAP_TOP = top as usize;
    }

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
    #[packed_field(size_bits = "64", endian = "msb")]
    pub(crate) drop_vptr: Integer<u64, packed_bits::Bits63>,
}

impl BlockMetadata {
    fn new(size: usize, is_gc: bool) -> Self {
        Self {
            size: (size as u64).into(),
            is_gc,
            mark_bit: false,
            drop_vptr: (ptr::null_mut::<u8>() as u64).into(),
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

/// A zero-cost wrapper around a pointer to an allocation block.
///
/// This interface is used by the rest of the collector to access header
/// information of a block.
///
/// # Safety
///
/// This interface is inherently unsafe, it is no more than a wrapper around a
/// raw pointer and makes no guarnatees about the liveness of the block it
/// represents. Use of the `Block` API without first establishing that the
/// corresponding allocation block is live can lead to use-after-frees.
///
/// This API is safe to use on uninitialized blocks, as a Block's header is
/// always constructed before a block pointer is returned to the application.
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Block<'a> {
    base: *mut u8,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> Block<'a> {
    pub(crate) fn new(ptr: *mut u8) -> Self {
        Self {
            base: ptr,
            _lifetime: PhantomData,
        }
    }

    pub fn range(&self) -> ::std::ops::Range<usize> {
        let start = self.base as usize;
        let end = self.base as usize + *self.header().metadata().size as usize;
        ::std::ops::Range { start, end }
    }

    pub(crate) fn find<'mrk>(word: usize, ctxt: &MarkingCtxt<'mrk>) -> Option<Block<'mrk>> {
        // Since the heap starts at the end of the data segment, we can use this
        // as the lower heap bound.
        if word >= ctxt.data_segment_end && word <= unsafe { HEAP_TOP } {
            ALLOCATOR.iter()
                // It's legal to hold a pointer 1 byte past the end of a block,
                // but we can still use find because this will never over-run
                // the size of the next block's header.
                .find(|x| {
                    let addr = x.base as usize;
                    word == addr || word > addr && word <= addr + *x.header().metadata().size as usize
                })
        } else {
            None
        }
    }

    pub(crate) fn header(&self) -> &BlockHeader {
        unsafe { &*(self.base as *const Block as *const BlockHeader).sub(1) }
    }

    pub(crate) fn header_mut(&mut self) -> &mut BlockHeader {
        unsafe { &mut *(self.base as *mut Block as *mut BlockHeader).sub(1) }
    }

    pub(crate) fn colour(&self) -> Colour {
        let md = self.header().metadata();
        if md.mark_bit {
            Colour::Black
        } else {
            Colour::White
        }
    }

    pub(crate) fn set_colour(&mut self, colour: Colour) {
        let mut md = self.header().metadata();
        match colour {
            Colour::Black => md.mark_bit = true,
            Colour::White => md.mark_bit = false,
        }
        self.header_mut().set_metadata(md);
    }

    pub(crate) fn drop_vptr(&self) -> *mut u8 {
        let vptr = *self.header().metadata().drop_vptr as u64;
        vptr as usize as *mut u8
    }

    pub(crate) fn set_drop_vptr(&mut self, value: *mut u8) {
        let mut md = self.header().metadata();
        md.drop_vptr = (value as u64).into();
        self.header_mut().set_metadata(md);
    }

    pub(crate) unsafe fn drop(self) {
        debug_assert!(
            self.header().metadata().is_gc,
            "Can only call drop on Gc values"
        );
        let vptr = self.drop_vptr();
        let fatptr: &mut dyn Drop = ::std::mem::transmute((self.base, vptr));
        let size = ::std::mem::size_of_val(fatptr);
        let align = ::std::mem::align_of_val(fatptr);
        let layout = Layout::from_size_align_unchecked(size, align);

        ::std::ptr::drop_in_place(fatptr);
        crate::GC_ALLOCATOR.dealloc(NonNull::new_unchecked(self.base), layout);
    }
}

unsafe impl<'a> Send for Block<'a> {}
unsafe impl<'a> Sync for Block<'a> {}

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
            _lifetime: PhantomData,
        }
    }
}

pub(crate) struct BlocksIter<'a> {
    idx: usize,
    next: *mut BlockHeader,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> Iterator for BlocksIter<'a> {
    type Item = Block<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next.is_null() {
            return None;
        }

        let header = unsafe { ptr::read(self.next) };
        let res = unsafe { Block::new(self.next.add(1) as *mut u8) };

        self.idx += 1;
        self.next = header.prev;

        Some(res)
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
