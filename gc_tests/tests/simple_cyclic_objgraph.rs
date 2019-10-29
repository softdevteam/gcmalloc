// Run-time:
//   status: success

extern crate gcmalloc;

use gcmalloc::{collect, gc::DebugFlags, Debug, Gc};

struct Node {
    data: String,
    edge: Option<Gc<Node>>,
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
    gcmalloc::init(DebugFlags::new().mark_only());

    let a = make_objgraph();
    gcmalloc::collect();

    // Test a
    assert_eq!(a.data, String::from("a"));
    assert!(Debug::is_black(a));

    // Test b
    assert_eq!(a.edge.unwrap().data, String::from("b"));
    assert!(Debug::is_black(a.edge.unwrap()));

    // Test c
    assert_eq!(a.edge.unwrap().edge.unwrap().data, String::from("c"));
    assert!(Debug::is_black(a.edge.unwrap().edge.unwrap()));

    // Test c -> a
    assert_eq!(
        a.edge.unwrap().edge.unwrap().edge.unwrap().data,
        String::from("a")
    );
    assert!(Debug::is_black(a.edge.unwrap().edge.unwrap().edge.unwrap()));
}
