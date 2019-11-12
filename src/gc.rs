use crate::{
    alloc::{AllocMetadata, PtrInfo},
    Gc, GC_ALLOCATOR,
};
use std::{
    alloc::{Alloc, Layout},
    ptr::NonNull,
    sync::Mutex,
};

static WORD_SIZE: usize = std::mem::size_of::<usize>(); // in bytes

type Address = usize;

type Word = usize;

type StackScanCallback = extern "sysv64" fn(&mut Collector, Address);
#[link(name = "SpillRegisters", kind = "static")]
extern "sysv64" {
    // Pass a type-punned pointer to the collector and move it to the asm spill
    // code. This is so it can be passed straight back as the implicit `self`
    // address in the callback.
    #[allow(improper_ctypes)]
    fn spill_registers(collector: *mut u8, callback: StackScanCallback);
}

/// Used to denote which state the collector is in at a given point in time.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CollectorState {
    Ready,
    RootScanning,
    Marking,
    Sweeping,
}

/// Flags which affect the garbage collector's behaviour. They are useful for
/// isolating certain phases of a collection for testing or debugging purposes.
///
/// The flags are passed to the `init` function which initializes the
/// collector.
pub struct DebugFlags {
    pub mark_phase: bool,
    pub sweep_phase: bool,
}

impl DebugFlags {
    pub fn new() -> Self {
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
    worklist: Vec<PtrInfo>,

    /// The value of the mark-bit which the collector uses to denote whether an
    /// object is black (marked). As this can change after each collection, its
    /// current state needs storing.
    black: bool,

    /// Flags used to turn on/off certain collection phases for debugging &
    /// testing purposes.
    pub(crate) debug_flags: DebugFlags,

    /// The current state that the collector is in. Some operations can only be
    /// performed in specific states.
    pub(crate) state: Mutex<CollectorState>,
}

impl Collector {
    pub(crate) fn new(debug_flags: DebugFlags) -> Self {
        Self {
            worklist: Vec::new(),
            black: true,
            debug_flags,
            state: Mutex::new(CollectorState::Ready),
        }
    }

    pub(crate) fn current_black(&self) -> bool {
        self.black
    }

    /// The only entry-point to a collection. All collection phases must be
    /// triggered through this method. It is UB to call any of them
    /// individually.
    pub(crate) fn collect(&mut self) {
        // First check that no call to collect is active
        {
            let mut cstate = self.state.lock().unwrap();
            match *cstate {
                CollectorState::Ready => *cstate = CollectorState::RootScanning,
                _ => {
                    // The collector is running on another thread.
                    return;
                }
            }
        }

        // Register spilling is platform specific. This is implemented in
        // an assembly stub. The fn to scan the stack is passed as a callback
        unsafe { spill_registers(self as *mut Collector as *mut u8, Collector::scan_stack) }

        if self.debug_flags.mark_phase {
            self.enter_mark_phase();
        }

        if self.debug_flags.sweep_phase {
            self.enter_sweep_phase();
        }

        *self.state.lock().unwrap() = CollectorState::Ready;
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
        *self.state.lock().unwrap() = CollectorState::Marking;

        while !self.worklist.is_empty() {
            let PtrInfo { ptr, size, gc } = self.worklist.pop().unwrap();

            if gc {
                // For GC objects, the pointer recorded in the alloc metadata
                // list points to the beginning of the object -- *not* the
                // object's header. This means that unlike regular allocations,
                // `ptr` will never point to the beginning of the allocation
                // block.
                let obj = unsafe { Gc::from_raw(ptr as *const i8) };
                if self.colour(obj) == Colour::Black {
                    continue;
                }
                self.mark(obj, Colour::Black);
            }

            // Check each word in the allocation block for pointers.
            for addr in (ptr..ptr + size).step_by(WORD_SIZE) {
                let word = unsafe { *(addr as *const Word) };

                if let Some(ptrinfo) = AllocMetadata::find(word) {
                    self.worklist.push(ptrinfo)
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
        *self.state.lock().unwrap() = CollectorState::Sweeping;

        for PtrInfo { ptr, .. } in AllocMetadata.iter().filter(|x| x.gc) {
            let obj = unsafe { Gc::from_raw(ptr as *const i8) };
            if self.colour(obj) == Colour::White {
                unsafe {
                    let baseptr = (ptr as *mut u8).sub(obj.base_ptr_offset());
                    GC_ALLOCATOR.dealloc(
                        NonNull::new_unchecked(baseptr as *mut u8),
                        Layout::new::<usize>(),
                    );
                }
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
            if let Some(ptr_info) = AllocMetadata::find(stack_word) {
                self.worklist.push(ptr_info)
            }
        }
    }

    pub(crate) fn colour(&self, obj: Gc<i8>) -> Colour {
        if obj.mark_bit() == self.black {
            Colour::Black
        } else {
            Colour::White
        }
    }

    fn mark(&self, mut obj: Gc<i8>, colour: Colour) {
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
