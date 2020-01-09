// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, DebugFlags, Debug, Gc};

fn main() {
    let threshold = 5;
    // Lower the threshold so that the test doesn't take forever.
    gcmalloc::set_threshold(threshold);

    let x = Gc::new("Hello World".to_string());
    assert!(!Debug::is_black(
        Gc::into_raw(x.clone()) as *mut String as *mut u8
    ));

    for i in 0..threshold {
        let x = Gc::new(123 as usize);
    }

    assert!(Debug::is_black(Gc::into_raw(x) as *mut String as *mut u8));
}
