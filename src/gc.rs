use std::{
    alloc::{Alloc, Layout},
    any::Any,
    fmt,
    marker::Unsize,
    mem::{forget, transmute, ManuallyDrop},
    ops::{CoerceUnsized, Deref, DerefMut},
    ptr::NonNull,
};

use crate::allocator::Block;

use crate::GC_ALLOCATOR;

/// A garbage collected pointer. 'Gc' stands for 'Garbage collected'.
///
/// The type `Gc<T>` provides shared ownership of a value of type `T`,
/// allocted in the heap. `Gc` pointers are `Copyable`, so new pointers to
/// the same value in the heap can be produced trivially. The lifetime of
/// `T` is tracked automatically: it is freed when the application
/// determines that no references to `T` are in scope. This does not happen
/// deterministically, and no guarantees are given about when a value
/// managed by `Gc` is freed.
///
/// Shared references in Rust disallow mutation by default, and `Gc` is no
/// exception: you cannot generally obtain a mutable reference to something
/// inside an `Gc`. If you need mutability, put a `Cell` or `RefCell` inside
/// the `Gc`.
///
/// Unlike `Rc<T>`, cycles between `Gc` pointers are allowed and can be
/// deallocated without issue.
///
/// `Gc<T>` automatically dereferences to `T` (via the `Deref` trait), so
/// you can call `T`'s methods on a value of type `Gc<T>`.
///
/// `Gc<T>` is implemented using a tracing mark-sweep garbage collection
/// algorithm. This means that by using `Gc` pointers in a Rust application,
/// you pull in the overhead of a run-time garbage collector to manage and
/// free `Gc` values behind the scenes.
#[derive(PartialEq, Eq, Debug)]
pub struct Gc<T: ?Sized> {
    pub(crate) objptr: NonNull<GcBox<T>>,
}

impl<T> Gc<T> {
    /// Constructs a new `Gc<T>`.
    pub fn new(v: T) -> Self {
        Gc {
            objptr: unsafe { NonNull::new_unchecked(GcBox::new(v)) },
        }
    }
}

impl Gc<dyn Any> {
    pub fn downcast<T: Any>(&self) -> Result<Gc<T>, Gc<dyn Any>> {
        if (*self).is::<T>() {
            let ptr = self.objptr.cast::<GcBox<T>>();
            forget(self);
            Ok(Gc { objptr: ptr })
        } else {
            Err(Gc {
                objptr: self.objptr,
            })
        }
    }
}

impl<T: ?Sized> Gc<T> {
    /// Get a raw pointer to the underlying value `T`.
    pub fn into_raw(this: Self) -> *const T {
        let ptr: *const T = &*this;
        ptr
    }

    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.objptr.as_ptr() == other.objptr.as_ptr()
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// A `GcBox` is a 0-cost wrapper which allows a single `Drop` implementation
/// while also permitting multiple, copyable `Gc` references. The `drop` method
/// on `GcBox` acts as a guard, preventing the destructors on its contents from
/// running unless the object is really dead.
pub(crate) struct GcBox<T: ?Sized>(ManuallyDrop<T>);

impl<T> GcBox<T> {
    fn new(value: T) -> *mut GcBox<T> {
        let layout = Layout::new::<T>();

        let ptr = unsafe { GC_ALLOCATOR.alloc(layout).unwrap().as_ptr() } as *mut GcBox<T>;
        let gcbox = GcBox(ManuallyDrop::new(value));
        unsafe {
            ptr.copy_from_nonoverlapping(&gcbox, 1);
        }

        forget(gcbox);

        unsafe {
            let fatptr: &dyn Drop = &*ptr;
            let vptr = transmute::<*const dyn Drop, (usize, *mut u8)>(fatptr).1;
            (*ptr).block().set_drop_vptr(vptr);
        }

        ptr
    }
}

impl<T: ?Sized> GcBox<T> {
    fn block(&self) -> Block {
        Block::new(self as *const GcBox<T> as *mut u8)
    }
}

impl<T: ?Sized> Deref for Gc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(self.objptr.as_ptr() as *const T) }
    }
}

impl<T: ?Sized> DerefMut for Gc<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *(self.objptr.as_ptr() as *mut T) }
    }
}

impl<T: ?Sized> Drop for GcBox<T> {
    fn drop(&mut self) {
        println!("Dropping GcBox");
        if self.block().colour() == Colour::Black {
            return;
        }
        unsafe { ManuallyDrop::drop(&mut self.0) };
    }
}

/// `Copy` and `Clone` are implemented manually because a reference to `Gc<T>`
/// should be copyable regardless of `T`. It differs subtly from `#[derive(Copy,
/// Clone)]` in that the latter only makes `Gc<T>` copyable if `T` is.
impl<T: ?Sized> Copy for Gc<T> {}

impl<T: ?Sized> Clone for Gc<T> {
    fn clone(&self) -> Self {
        Gc {
            objptr: self.objptr,
        }
    }
}

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Gc<U>> for Gc<T> {}

/// Colour of an object used during marking phase (see Dijkstra tri-colour
/// abstraction)
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum Colour {
    Black,
    White,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn test_trait_obj() {
        trait HelloWorld {
            fn hello(&self) -> usize;
        }

        struct HelloWorldStruct(usize);

        impl HelloWorld for HelloWorldStruct {
            fn hello(&self) -> usize {
                self.0
            }
        }

        let s = HelloWorldStruct(123);
        let gcto: Gc<dyn HelloWorld> = Gc::new(s);
        assert_eq!(size_of::<Gc<dyn HelloWorld>>(), 2 * size_of::<usize>());
        assert_eq!(gcto.hello(), 123);
    }

    #[test]
    fn test_unsized() {
        let foo: Gc<[i32]> = Gc::new([1, 2, 3]);
        assert_eq!(foo, foo.clone());
    }

    #[test]
    fn test_nonnull_opt() {
        assert_eq!(size_of::<Option<Gc<usize>>>(), size_of::<usize>())
    }
}
