# gcmalloc

A Rust allocator with support for conservative garbage collection.

## The Drop Trait

`Gc` values implement the `Drop` trait. Unlike non-garbage-collected values,
however, the `drop` method is not called when a `Gc` value goes out of scope.
Instead, once the collector has determined that a value is garbage, it is added
to a drop queue, where its `drop` method is ran at an indeterminate point in the
future. This means that `Drop` now acts as both a regular destructor in the
presence of scoped values, and a garbage collection finalizer when applied to
`Gc` values.

As with regular destructors, `drop` methods on `Gc` values are run from the
"outside-in". However, if there are cycles, there is no way to know which `drop`
method is safe to run first. Consider the example below:

```rust

struct Node<T> {
    edge: Option<Gc<T>>
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        println!("Points to: {}", self.edge.unwrap())
    }
}

let x = Gc::new(Node { edge: None })
let y = Gc::new(Node { edge: None })

x.edge = Some(y);
y.edge = Some(x);

```

Since an object is deallocated after its `drop` method is run, there is no safe
order to run these destructors. If `x`'s `drop` method is called first, dropping
`y` will dereference a dangling pointer. The same is true in the inverse order.

To solve this, we impose an additional restriction on the `Drop` trait: you must
not dereference a `Gc` inside the `drop` method. This works because unsafe
`Drop` cycles can only be created using `Gc`s. By forbidding `drop` from
derefing through `Gc`s, we can ensure `drop` only has access to resources which
are guaranteed to be live.
