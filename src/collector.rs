use std::{alloc::Layout, mem::transmute};

use crate::{
    allocator::{AllocCache, Block, GcVec, HEAP_TOP},
    gc::Colour,
    ALLOCATOR, COLLECTOR_PHASE, GC_ALLOCATION_THRESHOLD,
};

static WORD_SIZE: usize = std::mem::size_of::<usize>(); // in bytes

type Address = usize;

type Word = usize;

/// Used to denote which phase the collector is in at a given point in time.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CollectorPhase {
    Ready,
    Preparation,
    Marking,
    Sweeping,
    Finalization,
}

impl CollectorPhase {
    fn update(&mut self, new_phase: CollectorPhase) {
        match (*self, new_phase) {
            (CollectorPhase::Ready, CollectorPhase::Preparation) => *self = new_phase,
            (_, CollectorPhase::Preparation) => {
                panic!("Invalid GC phase transition: {:?} -> {:?}", self, new_phase)
            }

            (_, _) => *self = new_phase,
        }
        *self = new_phase
    }
}

/// Flags which affect the garbage collector's behaviour. They are useful for
/// isolating certain phases of a collection for testing or debugging purposes.
///
/// The flags are passed to the `init` function which initializes the
/// collector.
#[derive(Debug)]
pub struct DebugFlags {
    pub prep_phase: bool,
    pub mark_phase: bool,
    pub sweep_phase: bool,
    pub automatic_collection: bool,
}

impl DebugFlags {
    pub const fn new() -> Self {
        Self {
            prep_phase: true,
            mark_phase: true,
            sweep_phase: true,
            automatic_collection: true,
        }
    }

    pub fn prep_phase(mut self, val: bool) -> Self {
        self.prep_phase = val;
        self
    }

    pub fn mark_phase(mut self, val: bool) -> Self {
        self.mark_phase = val;
        self
    }

    pub fn sweep_phase(mut self, val: bool) -> Self {
        self.sweep_phase = val;
        self
    }
}

/// Contains data expected to last for the duration of a single collection
/// cycle.
struct CollectionCtxt<'a> {
    /// The allocation cache is a freeze-frame of all block pointers in-use by
    /// the mutator. It is a sorted vector which allows for fast pointer
    /// identification using binary search. It is valid up until the drop_phase
    /// of a collection.
    allocation_cache: AllocCache<'a>,
    /// The data segment of an ELF binary contains statics. This section needs
    /// scanning during marking as it could contain GC roots.
    pub(crate) data_segment_start: usize,
    pub(crate) data_segment_end: usize,
}

impl<'a> CollectionCtxt<'a> {
    pub(crate) fn find(&mut self, word: usize) -> Option<Block<'a>> {
        if !(word >= self.data_segment_end && word <= unsafe { HEAP_TOP }) {
            return None;
        }
        self.allocation_cache.find(word)
    }
}

/// Holds information needed for the mark phase of GC
pub(crate) struct MarkingCtxt<'mrk, 'col: 'mrk> {
    /// The worklist holds references to heap blocks which were identified by
    /// potential pointers somewhere in the object graph. During marking, the
    /// entire worklist is traversed in order for a full transitive closure over
    /// the object graph to be performed.
    pub(crate) worklist: GcVec<Block<'mrk>>,
    /// The top (or start) of the current thread's stack.
    pub(crate) stack_start: usize,
    collection_ctxt: &'mrk mut CollectionCtxt<'col>,
}

type StackScanCallback = unsafe extern "sysv64" fn(*mut MarkingCtxt, Address);
#[link(name = "SpillRegisters", kind = "static")]
extern "sysv64" {
    // Pass a type-punned pointer to the collector and move it to the asm spill
    // code. This is so it can be passed straight back as the implicit `self`
    // address in the callback.
    #[allow(improper_ctypes)]
    fn spill_registers(ctxt: *mut MarkingCtxt, callback: StackScanCallback);
}

impl<'mrk, 'col> MarkingCtxt<'mrk, 'col> {
    /// Scans the stack from bottom to top, starting from the position of the
    /// current stack pointer. This method should never be called directly.
    /// Instead, it should be invoked as a callback from a platform specific
    /// assembly stub which is expected to get the contents of the stack pointer
    /// and spill all registers which may contain roots.
    #[no_mangle]
    unsafe extern "sysv64" fn scan_stack(ctxt: *mut MarkingCtxt, rsp: Address) {
        let ctxt = &mut *ctxt;
        for stack_address in (rsp..ctxt.stack_start).step_by(WORD_SIZE) {
            let stack_word = *(stack_address as *const Word);
            if let Some(block) = ctxt.collection_ctxt.find(stack_word) {
                ctxt.worklist.push(block)
            }
        }
    }

    fn mark(&mut self) {
        // Register spilling is platform specific. This is implemented in
        // an assembly stub, with scan_stack passed as a callback.
        unsafe { spill_registers(self, MarkingCtxt::scan_stack) }

        self.scan_statics();
        self.process_worklist();
    }

    /// Roots can hide inside static variables, so these need scanning for
    /// potential pointers too.
    #[cfg(target_os = "linux")]
    fn scan_statics(&mut self) {
        for data_addr in (self.collection_ctxt.data_segment_start
            ..self.collection_ctxt.data_segment_end)
            .step_by(WORD_SIZE)
        {
            let static_word = unsafe { *(data_addr as *const Word) };
            if let Some(block) = self.collection_ctxt.find(static_word) {
                self.worklist.push(block)
            }
        }
    }

    fn process_worklist(&mut self) {
        while !self.worklist.is_empty() {
            let mut block = self.worklist.pop().unwrap();
            if block.colour() == Colour::Black {
                continue;
            }

            block.set_colour(Colour::Black);

            // Check each word in the allocation block for pointers.
            for addr in block.range().step_by(WORD_SIZE) {
                let word = unsafe { *(addr as *const Word) };
                if let Some(block) = self.collection_ctxt.find(word) {
                    self.worklist.push(block)
                }
            }
        }
    }
}

/// A collector responsible for finding and freeing unreachable objects.
///
/// It is implemented as a stop-the-world, conservative, mark-sweep GC. A full
/// collection can broken down into 4 distinct phases:
///
/// 1) Preparation Phase - A single pass over the heap is performed where the
///    mark-bit of each GC object is cleared, indicating that they are
///    potentially unreachable.
///
/// 2) Mark phase - A transitive closure over the object graph is performed to
///    determine which garbage-collected objects are reachable. This is started
///    from on-stack and in-value registers at the time of collection - known as
///    the root-set. All potential pointers - regardless of memory management
///    strategy - are traced in search of further on-heap-pointers. Once
///    reached, garbage-collected objects are marked *black* to denote that they
///    are reachable.
///
/// 3) Sweep phase - The heap is scanned for all white garbage-collected
///    objects, which, when found, are added to the drop_queue to be finalized
///    and deallocated.
///
/// 4) Finalization phase - Each white object in the drop queue is finalized,
///    before having its memory deallocated.
///
/// During a collection, each phase is run consecutively and requires all
/// mutator threads to come to a complete stop.
pub(crate) struct Collector {
    /// Holds pointers to unreachable blocks awaiting destruction.
    drop_queue: GcVec<Block<'static>>,

    /// Flags used to turn on/off certain collection phases for debugging &
    /// testing purposes.
    pub(crate) debug_flags: DebugFlags,

    /// The number of GC values allocated before a collection is triggered.
    pub(crate) allocation_threshold: usize,

    /// The number of GC values allocated since the last collection.
    pub(crate) allocations: usize,
}

impl<'m, 's: 'm> Collector {
    pub(crate) const fn new() -> Self {
        Self {
            drop_queue: GcVec::new(),
            debug_flags: DebugFlags::new(),
            allocation_threshold: GC_ALLOCATION_THRESHOLD,
            allocations: 0,
        }
    }

    /// Asks the collector if it needs to collect garbage. If it does, a STW
    /// collection takes place, before resuming the mutator where it left off.
    pub(crate) fn poll(&mut self) {
        if self.debug_flags.automatic_collection && self.allocations >= self.allocation_threshold {
            self.collect();
        }
    }

    /// The only entry-point to a collection. All collection phases must be
    /// triggered through this method. It is UB to call any of them
    /// individually.
    pub(crate) fn collect(&mut self) {
        let allocation_cache = ALLOCATOR.sorted_cache();
        let (data_segment_start, data_segment_end) = unsafe { get_data_segment_range() };
        let mut colcx = CollectionCtxt {
            allocation_cache,
            data_segment_start,
            data_segment_end,
        };

        if self.debug_flags.prep_phase {
            self.enter_preparation_phase(&mut colcx);
        }

        COLLECTOR_PHASE.lock().update(CollectorPhase::Marking);

        if self.debug_flags.mark_phase {
            self.enter_mark_phase(&mut colcx);
        }

        if self.debug_flags.sweep_phase {
            self.enter_sweep_phase();
        }

        self.enter_drop_phase();

        self.allocations = 0;

        COLLECTOR_PHASE.lock().update(CollectorPhase::Ready);
    }

    /// The entry-point to the preperation phase.
    ///
    /// This sets the mark-bits of all garbage-collected objects to white.
    fn enter_preparation_phase(&mut self, col: &mut CollectionCtxt) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::Preparation);
        for block in col.allocation_cache.iter_mut() {
            block.set_colour(Colour::White)
        }
    }

    /// The entry-point to the mark phase.
    ///
    /// Calling this method assumes that the marking worklist has already been
    /// populated with blocks pointed to from the root-set.
    ///
    /// The mark phase colours blocks in the worklist which are managed by the
    /// collector. It also traverses the contents of **all** blocks for further
    /// pointers.
    fn enter_mark_phase(&mut self, colcx: &'m mut CollectionCtxt<'s>) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::Marking);

        let stack_start = unsafe { get_stack_start().unwrap() };

        let mut ctxt = MarkingCtxt {
            worklist: GcVec::new(),
            stack_start,
            collection_ctxt: colcx,
        };

        ctxt.mark();
    }

    /// The entry-point to the sweep phase.
    ///
    /// It is assumed that the sweep phase happens immediately after marking.
    /// All white objects (i.e. objects which were not marked) are deallocated.
    ///
    /// If this method is called without the marking phase being called first,
    /// then all gc-managed objects are presumed dead and deallocated.
    fn enter_sweep_phase(&mut self) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::Sweeping);

        let dummy_vptr = {
            let dummy = &crate::gc::GcDummyDrop {};
            let fatptr: &dyn Drop = &*dummy;
            unsafe { transmute::<*const dyn Drop, (usize, *mut u8)>(fatptr).1 }
        };

        for block in ALLOCATOR.iter() {
            if block.header().metadata().is_gc && block.colour() == Colour::White {
                if block.drop_vptr() != dummy_vptr {
                    self.drop_queue.push(block);
                    continue;
                }

                // Since we are certain that T has no need for finalization, we
                // can just dealloc it in-place. We also do not need to provide
                // a legitimate layout, as we use System alloc/dealloc calls
                // which require only the base pointer.
                unsafe { block.dealloc(Layout::new::<usize>()) };
            }
        }
    }

    /// The entry-point to the drop phase.
    ///
    /// Iterates over the objects in the drop queue and run their destructors.
    /// Since Rust's `Drop` trait is used, the drop semantics should be familiar
    /// to Rust users: destructors are run from the outside-in. In the case of
    /// cycles between `Gc` objects, no guarantees are made about which
    /// destructor is ran first.
    fn enter_drop_phase(&mut self) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::Finalization);

        while !self.drop_queue.is_empty() {
            let block = self.drop_queue.pop().unwrap();
            unsafe { block.drop() }
        }
    }
}

/// Attempt to get the starting address of the stack via the pthread API. This
/// is highly platform specific. It is considered the top-of-stack value during
/// stack root scanning.
#[cfg(target_os = "linux")]
unsafe fn get_stack_start() -> Option<Address> {
    let mut attr: libc::pthread_attr_t = std::mem::zeroed();
    assert_eq!(libc::pthread_attr_init(&mut attr), 0);
    let ptid = libc::pthread_self();
    let e = libc::pthread_getattr_np(ptid, &mut attr);
    if e != 0 {
        assert_eq!(libc::pthread_attr_destroy(&mut attr), 0);
        return None;
    }
    let mut stackaddr = std::ptr::null_mut();
    let mut stacksize = 0;
    assert_eq!(
        libc::pthread_attr_getstack(&attr, &mut stackaddr, &mut stacksize),
        0
    );
    return Some((stackaddr as usize + stacksize) as Address);
}

#[cfg(target_os = "linux")]
unsafe fn get_data_segment_range() -> (usize, usize) {
    extern "C" {
        static __data_start: libc::c_int;
        static end: libc::c_int;
    }

    // On some platforms, __data_start is not available. It's not possible to
    // switch this with __etext without including a custom SIGSEGV handler as
    // there may be unmapped pages between _etext and _end which we would hit
    // when scanning.
    let s = &__data_start as *const _ as *const u8;
    let start = s.add(s.align_offset(WORD_SIZE)) as usize;

    // On most Unix systems, _end is a symbol included by the linker which
    // points to the first address past the .bss segment
    let end_ = &end as *const _ as usize;

    (start, end_)
}
