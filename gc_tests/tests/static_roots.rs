// Run-time:
//  status: success

extern crate gcmalloc;

use gcmalloc::{collect, gc::DebugFlags, Debug, Gc};

static mut SOME_ROOT: Option<Gc<String>> = None;

#[inline(never)]
fn setup_root() {
    unsafe{ println!("SOME_ROOT: {:?}", &SOME_ROOT as *const _ ) };
    unsafe { SOME_ROOT = Some(Gc::new("hello world".to_string()))};

}

fn main() {
    // We prevent the GC from reclaiming memory because *if* there's a bug and
    // the root was erronously not discovered, it would manifest as a
    // use-after-free which is trickier to debug.
    gcmalloc::debug_flags(DebugFlags::new().sweep_phase(false));

    // This is flakey.
    setup_root();

    gcmalloc::collect();
    unsafe {assert!(Debug::is_black(SOME_ROOT.unwrap().as_ptr() as *mut u8)) };
}
