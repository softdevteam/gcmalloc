use crate::{
    alloc::{GcVec, PtrInfo},
    GcBox, ALLOCATOR, COLLECTOR_PHASE, GC_ALLOCATOR,
};

use std::{
    alloc::{Alloc, Layout},
    mem::{align_of_val, size_of_val, transmute},
    ptr::NonNull,
};

static WORD_SIZE: usize = std::mem::size_of::<usize>(); // in bytes

type Address = usize;

type Word = usize;

/// Use this type when we do not care about the contents of a Gc. We choose `u8`
/// because it maps similarly to how C / C++ use char when dealing with raw
/// bytes.
pub(crate) struct OpaqueU8(u8);

type StackScanCallback = extern "sysv64" fn(&mut Collector, Address);
#[link(name = "SpillRegisters", kind = "static")]
extern "sysv64" {
    // Pass a type-punned pointer to the collector and move it to the asm spill
    // code. This is so it can be passed straight back as the implicit `self`
    // address in the callback.
    #[allow(improper_ctypes)]
    fn spill_registers(collector: *mut u8, callback: StackScanCallback);
}

/// Used to denote which phase the collector is in at a given point in time.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CollectorPhase {
    Ready,
    RootScanning,
    Marking,
    Sweeping,
    Finalization,
}

impl CollectorPhase {
    fn update(&mut self, new_phase: CollectorPhase) {
        match (*self, new_phase) {
            (CollectorPhase::Ready, CollectorPhase::RootScanning) => *self = new_phase,
            (_, CollectorPhase::RootScanning) => {
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
    pub mark_phase: bool,
    pub sweep_phase: bool,
}

impl DebugFlags {
    pub const fn new() -> Self {
        Self {
            mark_phase: true,
            sweep_phase: true,
        }
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

/// Colour of an object used during marking phase (see Dijkstra tri-colour
/// abstraction)
#[derive(PartialEq, Eq)]
pub(crate) enum Colour {
    Black,
    White,
}

/// A collector responsible for finding and freeing unreachable objects.
///
/// It is implemented as a stop-the-world, conservative, mark-sweep GC. A full
/// collection can broken down into 3 distinct phases:
///
/// 1) Stack Scanning - Upon entry to a collection, callee-save registers are
///    spilled to the call stack and it is scanned looking for on-heap-pointers.
///    If found, on-heap-pointers are added to the marking worklist ready for
///    the next phase.
///
/// 2) Mark phase - Each allocation block in the marking worklist is traced in
///    search of further on-heap-pointers. Allocation blocks which contain GC
///    objects are marked black. All blocks, regardless of memory management
///    strategy, are traced in search of further on-heap-pointers. This process
///    is repeated until the worklist is empty.
///
/// 3) Sweep phase - All GC objects which were not marked black in the mark
///    phase are deallocated as they are considered unreachable.
///
/// During a collection, each phase is run consecutively and requires all
/// mutator threads to come to a complete stop.
pub(crate) struct Collector {
    /// Used during the mark phase. The marking worklist consists of allocation
    /// blocks which still need tracing. Once the worklist is empty, the marking
    /// phase is complete, and the full object-graph has been traversed.
    worklist: GcVec<PtrInfo>,

    /// Holds pointers to unreachable objects awaiting destruction.
    drop_queue: GcVec<usize>,

    /// The value of the mark-bit which the collector uses to denote whether an
    /// object is black (marked). As this can change after each collection, its
    /// current state needs storing.
    black: bool,

    /// Flags used to turn on/off certain collection phases for debugging &
    /// testing purposes.
    pub(crate) debug_flags: DebugFlags,
}

impl Collector {
    pub(crate) const fn new() -> Self {
        Self {
            worklist: GcVec::new(),
            drop_queue: GcVec::new(),
            black: true,
            debug_flags: DebugFlags::new(),
        }
    }

    /// The only entry-point to a collection. All collection phases must be
    /// triggered through this method. It is UB to call any of them
    /// individually.
    pub(crate) fn collect(&mut self) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::RootScanning);

        // Register spilling is platform specific. This is implemented in
        // an assembly stub. The fn to scan the stack is passed as a callback
        unsafe { spill_registers(self as *mut Collector as *mut u8, Collector::scan_stack) }

        if self.debug_flags.mark_phase {
            self.enter_mark_phase();
        }

        if self.debug_flags.sweep_phase {
            self.enter_sweep_phase();
        }

        self.enter_drop_phase();

        COLLECTOR_PHASE.lock().update(CollectorPhase::Ready);
    }

    /// The entry-point to the mark phase.
    ///
    /// Calling this method assumes that the marking worklist has already been
    /// populated with blocks pointed to from the root-set.
    ///
    /// The mark phase colours blocks in the worklist which are managed by the
    /// collector. It also traverses the contents of **all** blocks for further
    /// pointers.
    fn enter_mark_phase(&mut self) {
        COLLECTOR_PHASE.lock().update(CollectorPhase::Marking);

        while !self.worklist.is_empty() {
            let PtrInfo { ptr, size, gc } = self.worklist.pop().unwrap();

            if gc {
                let obj = unsafe { &mut *(ptr as *mut GcBox<OpaqueU8>) };
                if self.colour(obj) == Colour::Black {
                    continue;
                }
                self.mark(obj, Colour::Black);
            }

            // Check each word in the allocation block for pointers.
            for addr in (ptr..ptr + size).step_by(WORD_SIZE) {
                let word = unsafe { *(addr as *const Word) };

                if let Some(block) = ALLOCATOR
                    .iter()
                    .find(|x| x.ptr == word || word > x.ptr && word < x.ptr + x.size)
                {
                    self.worklist.push(block)
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

        for block in ALLOCATOR.iter().filter(|x| x.gc) {
            let obj = block.ptr as *mut GcBox<OpaqueU8>;
            if self.colour(unsafe { &*obj }) == Colour::White {
                self.drop_queue.push(obj as usize);
            }
        }

        // Flip the meaning of the mark bit, i.e. if false == Black, then it
        // becomes false == white. This is a simplification which allows us to
        // avoid resetting the mark bit for every survived object after
        // collection. Since we do not implement a marking bitmap and instead
        // store this mark bit in each object header, this would be a very
        // expensive operation.
        self.black = !self.black;
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

        for i in self.drop_queue.iter().cloned() {
            let boxptr = i as *mut GcBox<OpaqueU8>;
            unsafe {
                let vptr = (&*boxptr).drop_vptr();
                let fatptr: &mut dyn Drop = transmute((boxptr, vptr));
                ::std::ptr::drop_in_place(fatptr);
            }
            self.dealloc(boxptr);
        }

        self.drop_queue = GcVec::new()
    }

    /// Free the contents of a Gc<T>. It is deliberately not a monomorphised
    /// because during collection, the concrete type of T is unknown. Instead,
    /// an arbitrary placeholder type is used to represent the contents as the
    /// actual type's size and alignment are dynamically fetched from the `Gc`s
    /// vtable.
    ///
    /// A raw pointer to a known `Gc<T>` can be passed to this method by
    /// creating a hollow `Gc<Placeholder>` struct with `Gc::from_raw`.
    /// `Gc::new()` must *not* be used.
    fn dealloc(&self, boxptr: *mut GcBox<OpaqueU8>) {
        let vptr = unsafe { (&*boxptr).drop_vptr() };
        let drop_trobj: &mut dyn Drop = unsafe { transmute((boxptr, vptr)) };
        let size = size_of_val(drop_trobj);
        let align = align_of_val(drop_trobj);

        unsafe {
            let layout = Layout::from_size_align_unchecked(size, align);
            let ptr = boxptr as *mut GcBox<OpaqueU8> as *mut u8;
            GC_ALLOCATOR.dealloc(NonNull::new_unchecked(ptr), layout);
        }
    }

    /// Scans the stack from bottom to top, starting from the position of the
    /// current stack pointer. This method should never be called directly.
    /// Instead, it should be invoked as a callback from a platform specific
    /// assembly stub which is expected to get the contents of the stack pointer
    /// and spill all registers which may contain roots.
    #[no_mangle]
    extern "sysv64" fn scan_stack(&mut self, rsp: Address) {
        let stack_top = unsafe { get_stack_start() }.unwrap();

        for stack_address in (rsp..stack_top).step_by(WORD_SIZE) {
            let stack_word = unsafe { *(stack_address as *const Word) };
            if let Some(block) = ALLOCATOR
                .iter()
                .find(|x| x.ptr == stack_word || stack_word > x.ptr && stack_word < x.ptr + x.size)
            {
                self.worklist.push(block);
            }
        }
    }

    pub(crate) fn colour(&self, obj: &GcBox<OpaqueU8>) -> Colour {
        if obj.metadata().mark_bit == self.black {
            Colour::Black
        } else {
            Colour::White
        }
    }

    pub(crate) fn mark(&self, obj: &mut GcBox<OpaqueU8>, colour: Colour) {
        match colour {
            Colour::Black => obj.set_mark_bit(self.black),
            Colour::White => obj.set_mark_bit(!self.black),
        };
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
