// Run-time:
//   status: success

extern crate gcmalloc;

use gcmalloc::{collect, gc::DebugFlags, Debug, Gc};

struct GcData {
    a: Gc<usize>,
    b: Gc<usize>,
}

struct OnRustHeap {
    a: Option<Box<OnRustHeap>>,
    b: Gc<GcData>,
}

fn make_objgraph() -> Box<OnRustHeap> {
    let x = GcData {
        a: Gc::new(10),
        b: Gc::new(20),
    };

    let y = GcData {
        a: Gc::new(30),
        b: Gc::new(40),
    };

    let inner_rh = OnRustHeap {
        a: None,
        b: Gc::new(x),
    };

    let outer_rh = Box::new(OnRustHeap {
        a: Some(Box::new(inner_rh)),
        b: Gc::new(y),
    });

    outer_rh
}

fn main() {
    gcmalloc::init(DebugFlags::new().mark_only());

    let objgraph = make_objgraph();
    gcmalloc::collect();

    let x = objgraph.a.unwrap().b;
    let y = objgraph.b;

    assert!(Debug::is_black(x));
    assert!(Debug::is_black(y));
}
