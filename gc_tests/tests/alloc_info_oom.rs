// Run-time:
//  status: error

extern crate gcmalloc;

static NUM_ALLOCATIONS: usize = 100000;

fn main() {
    let mut v = Vec::with_capacity(NUM_ALLOCATIONS);
    for i in 0..NUM_ALLOCATIONS {
        v.push(Box::new(i));
    }
}
