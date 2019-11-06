extern crate gcmalloc;

use gcmalloc::{collect, gc::DebugFlags, Debug, Gc};

struct GcData {
    a: Gc<usize>,
    b: Gc<Gc<usize>>,
}

impl GcData {
    fn new(a: Gc<usize>, b: Gc<Gc<usize>>) -> Self {
        Self { a, b }
    }
}

fn make_objgraph() -> Vec<GcData> {
    let mut gcs = Vec::new();
    for i in 1..1000 {
        gcs.push(GcData::new(Gc::new(i), Gc::new(Gc::new(1))))
    }
    gcs
}

fn main() {
    gcmalloc::init(DebugFlags::new().sweep_phase(false));

    let objgraph = make_objgraph();
    gcmalloc::collect();

    // for gcdata in objgraph.iter() {
    //     for i in gcdata.b.iter() {
    //         assert!(Debug::is_black(*i));
    //     }
    //     // assert!(Debug::is_black(gcdata.a));
    // }
}
