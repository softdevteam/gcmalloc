// Copyright (c) 2019 King's College London created by the Software Development
// Team <http://soft-dev.org/>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0>, or the MIT license <LICENSE-MIT
// or http://opensource.org/licenses/MIT>, or the UPL-1.0 license
// <http://opensource.org/licenses/UPL> at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use crate::{
    alloc::{AllocMetadata, PtrInfo},
    Gc,
};
use std::sync::Mutex;

static WORD_SIZE: usize = 8; // Bytes

type StackAddress = usize;
type StackValue = usize;

type StackScanCallback = extern "sysv64" fn(&mut Collector, StackAddress);
#[link(name = "SpillRegisters", kind = "static")]
extern "sysv64" {
    // Pass a type-punned pointer to the collector and move it to the asm spill
    // code. This is so it can be passed straight back as the implicit `self`
    // address in the callback.
    #[allow(improper_ctypes)]
    fn spill_registers(collector: *mut u8, callback: StackScanCallback);
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CollectorState {
    Ready,
    Marking,
    FinishedMarking,
}

pub struct DebugFlags {
    mark_only: bool,
}

impl DebugFlags {
    pub fn new() -> Self {
        Self { mark_only: false }
    }

    /// Prevents the GC from reclaiming 'garbage' objects. This flag is useful
    /// for debugging whether the collector was able to find an object(s) while
    /// traversing the object graph.
    pub fn mark_only(mut self) -> Self {
        self.mark_only = true;
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

pub(crate) struct Collector {
    worklist: Vec<PtrInfo>,
    black: bool,
    debug_flags: DebugFlags,
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

    pub(crate) fn collect(&mut self) {
        // First check that no call to collect is active
        {
            let mut cstate = self.state.lock().unwrap();
            match *cstate {
                CollectorState::Ready => *cstate = CollectorState::Marking,
                _ => {
                    // The collector is running on another thread.
                    return;
                }
            }
        }

        // Register spilling is platform specific. This is implemented in
        // an assembly stub. The fn to scan the stack is passed as a callback
        unsafe { spill_registers(self as *mut Collector as *mut u8, Collector::scan_stack) }

        self.enter_mark_phase();

        *self.state.lock().unwrap() = CollectorState::FinishedMarking;

        if self.debug_flags.mark_only {
            return;
        }

        // Flip the meaning of the mark bit, i.e. if false == Black, then it
        // becomes false == white. This is a simplification which allows us to
        // avoid resetting the mark bit for every survived object after
        // collection. Since we do not implement a marking bitmap and instead
        // store this mark bit in each object header, this would be a very
        // expensive operation.
        self.black = !self.black;
    }

    /// The worklist is populated with potential GC roots during the stack
    /// scanning phase. The mark phase then traces through this root-set until
    /// it finds GC objects. Once found, a GC object is coloured black to
    /// indicate that it is reachable by the mutator, and is therefore *not* a
    /// candidate for reclaimation.
    fn enter_mark_phase(&mut self) {
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
                    break;
                }

                self.mark(obj, Colour::Black);
            }
        }
    }

    #[no_mangle]
    extern "sysv64" fn scan_stack(&mut self, rsp: StackAddress) {
        let stack_start = unsafe { get_stack_start() }.unwrap() as usize;

        for stack_address in (rsp..stack_start).step_by(WORD_SIZE) {
            let stack_word = unsafe { *(stack_address as *const StackValue) };
            if let Some(ptr_info) = AllocMetadata::find(stack_word) {
                self.worklist.push(ptr_info)
            }
        }
    }

    pub(crate) fn colour(&self, obj: Gc<i8>) -> Colour {
        if obj.header().mark_bit() == self.black {
            Colour::Black
        } else {
            Colour::White
        }
    }

    fn mark(&self, obj: Gc<i8>, colour: Colour) {
        match colour {
            Colour::Black => obj.header_mut().set_mark_bit(self.black),
            Colour::White => obj.header_mut().set_mark_bit(!self.black),
        };
    }
}

/// Attempt to get the starting address of the stack via the pthread API. This
/// is highly platform specific. It is used as the lower bound for the range of
/// on-stack-values which are scanned for potential roots in GC.
#[cfg(target_os = "linux")]
unsafe fn get_stack_start() -> Option<usize> {
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
    return Some(stackaddr as usize + stacksize);
}
