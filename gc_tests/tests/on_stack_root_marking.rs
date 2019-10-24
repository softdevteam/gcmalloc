// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::Gc;
use gcmalloc::{collect, Debug };
use gcmalloc::gc::DebugFlags;

fn foo() {
    let y = Gc::new(456 as usize);
    collect();
    assert!(Debug::is_black(y));
}

fn main() {
    gcmalloc::init(DebugFlags::new().mark_only());

    let x = Gc::new(123 as usize);
    foo(); // triggers a collection
    assert!(Debug::is_black(x));
}
