// Run-time:
//   status: success

extern crate gcmalloc;

use gcmalloc::{collect, gc::DebugFlags, Debug, Gc};

static mut COUNTER: usize = 0;

struct Node {
    data: String,
    edge: Option<Gc<Node>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        unsafe { COUNTER += 1 }
    }
}

fn make_objgraph() -> Gc<Node> {
    let mut a = Gc::new(Node {
        data: "a".to_string(),
        edge: None,
    });
    let mut b = Gc::new(Node {
        data: "b".to_string(),
        edge: None,
    });
    let mut c = Gc::new(Node {
        data: "c".to_string(),
        edge: None,
    });

    a.edge = Some(b);
    b.edge = Some(c);
    c.edge = Some(a);

    a
}

fn main() {
    gcmalloc::init(DebugFlags::new().mark_phase(false));

    let a = make_objgraph();
    gcmalloc::collect();

    unsafe { assert_eq!(COUNTER, 3) };
}
