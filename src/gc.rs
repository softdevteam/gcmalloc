use std::{
    alloc::{Alloc, Layout},
    any::Any,
    fmt,
    marker::{PhantomData, Unsize},
    mem::{forget, transmute, ManuallyDrop, MaybeUninit},
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
    pub(crate) ptr: NonNull<GcBox<T>>,
    _phantom: PhantomData<T>,
}

impl<T> Gc<T> {
    /// Constructs a new `Gc<T>`.
    pub fn new(v: T) -> Self {
        Gc {
            ptr: unsafe { NonNull::new_unchecked(GcBox::new(v)) },
            _phantom: PhantomData,
        }
    }

    /// Constructs a new `Gc<MaybeUninit<T>>` which is capable of storing data
    /// up-to the size permissible by `layout`.
    ///
    /// This can be useful if you want to store a value with a custom layout,
    /// but have the collector treat the value as if it were T.
    ///
    /// `layout` must be at least as large as `T`, and have an alignment which
    /// is the same, or bigger than `T`.
    pub fn new_from_layout(layout: Layout) -> Option<Gc<MaybeUninit<T>>> {
        let tl = Layout::new::<T>();
        if layout.size() < tl.size() && layout.align() >= tl.align() {
            return None;
        }
        Some(Gc::from_inner(GcBox::new_from_layout(layout)))
    }
}

impl Gc<dyn Any> {
    pub fn downcast<T: Any>(&self) -> Result<Gc<T>, Gc<dyn Any>> {
        if (*self).is::<T>() {
            let ptr = self.ptr.cast::<GcBox<T>>();
            forget(self);
            Ok(Gc::from_inner(ptr))
        } else {
            Err(Gc::from_inner(self.ptr))
        }
    }
}

impl<T: ?Sized> Gc<T> {
    /// Get a raw pointer to the underlying value `T`.
    pub fn into_raw(this: Self) -> *const T {
        this.ptr.as_ptr() as *const T
    }

    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.ptr.as_ptr() == other.ptr.as_ptr()
    }

    pub fn from_raw(raw: *const T) -> Gc<T> {
        Gc {
            ptr: unsafe { NonNull::new_unchecked(raw as *mut GcBox<T>) },
            _phantom: PhantomData,
        }
    }

    fn from_inner(ptr: NonNull<GcBox<T>>) -> Self {
        Self {
            ptr,
            _phantom: PhantomData,
        }
    }
}

impl<T> Gc<MaybeUninit<T>> {
    /// As with `MaybeUninit::assume_init`, it is up to the caller to guarantee
    /// that the inner value really is in an initialized state. Calling this
    /// when the content is not yet fully initialized causes immediate undefined
    /// behavior.
    pub unsafe fn assume_init(self) -> Gc<T> {
        let ptr = self.ptr.as_ptr() as *mut GcBox<MaybeUninit<T>>;
        Gc::from_inner((&mut *ptr).assume_init())
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Used to synthesise a common vptr address which can be compared against for Gc
/// values which do not need dropping.
pub(crate) struct GcDummyDrop;

impl Drop for GcDummyDrop {
    fn drop(&mut self) {}
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

    fn new_from_layout(layout: Layout) -> NonNull<GcBox<MaybeUninit<T>>> {
        unsafe {
            let ptr = GC_ALLOCATOR.alloc(layout).unwrap().as_ptr() as *mut GcBox<MaybeUninit<T>>;

            let dummy_t = &layout as *const Layout as *const GcDummyDrop;
            let fatptr: &dyn Drop = &*dummy_t;
            let dummy_vptr = transmute::<*const dyn Drop, (usize, *mut u8)>(fatptr).1;
            (*ptr).block().set_drop_vptr(dummy_vptr);

            NonNull::new_unchecked(ptr)
        }
    }
}

impl<T> GcBox<MaybeUninit<T>> {
    unsafe fn assume_init(&mut self) -> NonNull<GcBox<T>> {
        // With T now considered initialized, we must make sure that if GcBox<T>
        // is reclaimed, T will be dropped. We need to find its vptr and replace the
        // GcDummyDrop vptr in the block header with it.
        let data = self.0.get_ref();
        let fatptr: &dyn Drop = &*(data as *const _ as *const GcBox<T>);
        let vptr = transmute::<*const dyn Drop, (usize, *mut u8)>(fatptr).1;
        self.block().set_drop_vptr(vptr);
        NonNull::new_unchecked(self as *mut _ as *mut GcBox<T>)
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
        unsafe { &*(self.ptr.as_ptr() as *const T) }
    }
}

impl<T: ?Sized> DerefMut for Gc<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *(self.ptr.as_ptr() as *mut T) }
    }
}

impl<T: ?Sized> Drop for GcBox<T> {
    fn drop(&mut self) {
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
        Gc::from_inner(self.ptr)
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
