use crate::{
    allocator::{Block, GcVec},
    gc::Colour,
    ALLOCATOR, COLLECTOR_PHASE, GC_ALLOCATION_THRESHOLD,
};

static WORD_SIZE: usize = std::mem::size_of::<usize>(); // in bytes

type Address = usize;

type Word = usize;

/// Use this type when we do not care about the contents of a Gc. We choose `u8`
/// because it maps similarly to how C / C++ use char when dealing with raw
/// bytes.
pub(crate) struct OpaqueU8(u8);

type StackScanCallback = unsafe extern "sysv64" fn(Address, *mut MarkingCtxt);
#[link(name = "SpillRegisters", kind = "static")]
extern "sysv64" {
    // Pass a type-punned pointer to the collector and move it to the asm spill
    // code. This is so it can be passed straight back as the implicit `self`
    // address in the callback.
    #[allow(improper_ctypes)]
    fn spill_registers(callback: StackScanCallback, ctxt: *mut MarkingCtxt);
}

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

/// Holds information needed for the mark phase of GC
pub(crate) struct MarkingCtxt<'mrk> {
    /// The worklist holds references to heap blocks which were identified by
    /// potential pointers somewhere in the object graph. During marking, the
    /// entire worklist is traversed in order for a full transitive closure over
    /// the object graph to be performed.
    pub(crate) worklist: GcVec<Block<'mrk>>,
    /// The top (or start) of the current thread's stack.
    pub(crate) stack_start: usize,
    /// The data segment of an ELF binary contains statics. This section needs
    /// scanning during marking as it could contain GC roots.
    pub(crate) data_segment_start: usize,
    pub(crate) data_segment_end: usize,
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

impl Collector {
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
        if self.debug_flags.prep_phase {
            self.enter_preparation_phase();
        }

        COLLECTOR_PHASE.lock().update(CollectorPhase::Marking);

        if self.debug_flags.mark_phase {
            self.enter_mark_phase();
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
    fn enter_preparation_phase(&mut self) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::Preparation);
        for mut block in ALLOCATOR.iter() {
            if block.header().metadata().is_gc {
                block.set_colour(Colour::White)
            }
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
    fn enter_mark_phase<'mrk>(&mut self) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::Marking);

        let stack_start = unsafe { get_stack_start().unwrap() };
        let (data_segment_start, data_segment_end) = unsafe { get_data_segment_range() };

        let mut ctxt = MarkingCtxt {
            worklist: GcVec::new(),
            stack_start,
            data_segment_start,
            data_segment_end,
        };

        // Register spilling is platform specific. This is implemented in
        // an assembly stub. The fn to scan the stack is passed as a callback
        unsafe { spill_registers(Collector::scan_stack, &mut ctxt) }

        self.scan_statics(&mut ctxt);
        self.mark_objects(&mut ctxt);
    }

    fn mark_objects<'mrk>(&mut self, ctxt: &mut MarkingCtxt<'mrk>) {
        while !ctxt.worklist.is_empty() {
            let mut block = ctxt.worklist.pop().unwrap();
            let md = block.header().metadata();

            if md.is_gc {
                if block.colour() == Colour::Black {
                    continue;
                }
                block.set_colour(Colour::Black);
            }

            // Check each word in the allocation block for pointers.
            for addr in block.range().step_by(WORD_SIZE) {
                let word = unsafe { *(addr as *const Word) };
                if let Some(block) = Block::find(word, ctxt) {
                    ctxt.worklist.push(block)
                }
            }
        }
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

        for block in ALLOCATOR.iter() {
            if block.header().metadata().is_gc && block.colour() == Colour::White {
                self.drop_queue.push(block)
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

    /// Scans the stack from bottom to top, starting from the position of the
    /// current stack pointer. This method should never be called directly.
    /// Instead, it should be invoked as a callback from a platform specific
    /// assembly stub which is expected to get the contents of the stack pointer
    /// and spill all registers which may contain roots.
    #[no_mangle]
    unsafe extern "sysv64" fn scan_stack<'mrk>(rsp: Address, ctxt: *mut MarkingCtxt<'mrk>) {
        let ctxt = &mut *ctxt;

        for stack_address in (rsp..ctxt.stack_start).step_by(WORD_SIZE) {
            let stack_word = *(stack_address as *const Word);
            if let Some(block) = Block::find(stack_word, ctxt) {
                ctxt.worklist.push(block)
            }
        }
    }

    /// Roots can hide inside static variables, so these need scanning for
    /// potential pointers too.
    #[cfg(target_os = "linux")]
    fn scan_statics<'mrk>(&mut self, ctxt: &mut MarkingCtxt<'mrk>) {
        for data_addr in (ctxt.data_segment_start..ctxt.data_segment_end).step_by(WORD_SIZE) {
            let static_word = unsafe { *(data_addr as *const Word) };
            if let Some(block) = Block::find(static_word, ctxt) {
                ctxt.worklist.push(block)
            }
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
