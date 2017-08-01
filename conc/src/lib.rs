//! # `conc` — An efficient concurrent reclamation system
//!
//! `conc` builds upon hazard pointers to create a extremely performant system for
//! concurrently handling memory. It is more general and convenient — and often also faster — than
//! epoch-based reclamation.
//!
//! ## Why?
//!
//! aturon's [blog post](https://aturon.github.io/blog/2015/08/27/epoch/) explains the issues of
//! concurrent memory handling very well, although it take basis in epoch-based reclamation, which
//! this crate is an alternative for.
//!
//! The gist essentially is that you need to delete objects in most concurrent data structure
//! (otherwise there would be memory leaks), however cannot safely do so, as there is no way to
//! know if another thread is accessing the object in question. This (and other reclamation
//! systems) provides a solution to this problem.
//!
//! ## Usage
//!
//! While the low-level API is available, it is generally sufficient to use the `conc::Atomic`
//! abstraction. This acts much like familiar Rust APIs. It allows the programmer to concurrently
//! access a value through references, as well as update it, and more. Refer to the respective docs
//! for more information.
//!
//! If you are interested in implementing your own structures with `conc`, you must learn how to
//! use `Guard` and `add_garbage`. In short,
//!
//! - `conc::add_garbage()` adds a destructor with a pointer, which will be run eventually, when no
//!   one is reading the data anymore. In other words, it acts as a concurrent counterpart to
//!   `Drop::drop()`.
//! - `Guard` "protects" a pointer from being destroyed. That is, it delays destruction (which is
//!   planned by `conc::add_garbage()`) until the guard is gone.
//!
//! See their respective API docs for details on usage and behavior.
//!
//! ### Debugging
//!
//! Enable feature `debug-tools` and set environment variable `CONC_DEBUG_MODE`. For example,
//! `CONC_DEBUG_MODE=1 cargo test --features debug-tools`. To get stacktraces after each message,
//! set environment variable `CONC_DEBUG_STACKTRACE`.
//!
//! ## Why not crossbeam/epochs?
//!
//! Epochs and classical hazard pointers are generally faster than this crate, but it doesn't
//! matter how fast it is, it has to be right.
//!
//! The issue with most other and faster solutions is that, if there is a non-trivial amount of
//! threads (say 16) constantly reading/storing some pointer, it will never get to a state, where
//! it can be reclaimed.
//!
//! In other words, given sufficient amount of threads and frequency, the gaps between the
//! reclamation might be very very long, causing very high memory usage, and potentially OOM
//! crashes.
//!
//! These issues are not hypothetical. It happened to me while testing the caching system of TFS.
//! Essentially, the to-be-destroyed garbage accumulated several gigabytes, without ever being open
//! to a collection cycle.
//!
//! It reminds of the MongoDB debate. It might very well be the fastest solution¹, but if it can't
//! even ensure consistency, what is the point?
//!
//! That being said, there are cases where this library is faster than the alternatives.
//! Moreover, there are cases where the other libraries are fine (e.g. if you have a bounded number
//! of thread and a medium-long interval between accesses).
//!
//! ¹If you want a super fast memory reclamation system, you should try NOP™, and not calling
//!  destructors.
//!
//! ## Internals
//!
//! It based on hazard pointers, although there are several differences. The idea is essentially
//! that the system keeps track of some number of "hazards". As long as a hazard protects some
//! object, the object cannot be deleted.
//!
//! Once in a while, a thread performs a garbage collection by scanning the hazards and finding the
//! objects not currently protected by any hazard. These objects are then deleted.
//!
//! To improve performance, we use a layered approach: Both garbage (objects to be deleted
//! eventually) and hazards are cached thread locally. This reduces the amount of atomic operations
//! and cache misses.
//!
//! ## Garbage collection
//!
//! Garbage collection of the concurrently managed object is done automatically between every `n`
//! frees where `n` is chosen from some probability distribution.
//!
//! Note that a garbage collection cycle might not clear all objects. For example, some objects
//! could be protected by hazards. Others might not have been exported from the thread-local cache
//! yet.
//!
//! ## Performance
//!
//! It is worth noting that atomic reads through this library usually requires three atomic CPU
//! instruction, this means that if you are traversing a list or something like that, this library
//! might not be for you.

#![feature(thread_local_state)]
#![deny(missing_docs)]

#[macro_use]
extern crate lazy_static;
extern crate rand;
extern crate spin;

mod atomic;
mod debug;
mod garbage;
mod global;
mod guard;
mod hazard;
mod local;
mod mpsc;
pub mod sync;

pub use atomic::Atomic;
pub use guard::Guard;

use std::mem;
use garbage::Garbage;

/// Attempt to collect garbage.
///
/// This function does two things:
///
/// 1. Export garbage from current thread to the global queue.
/// 2. Collect all the garbage and run destructors on the unused items.
///
/// If another thread is currently doing 2., it will be skipped. This makes it different from
/// `conc::gc()`, which will block.
///
/// If 2. fails (that is, another thread is garbage collecting), `Err(())` is returned. Otherwise
/// `Ok(())` is returned.
///
/// # Use case
///
/// Note that it is not necessary to call this manually, it will do so automatically after some
/// time has passed.
///
/// However, it can be nice if you have just trashed a very memory-hungry item in the current
/// thread, and want to attempt to GC it.
///
/// # Other threads
///
/// This cannot collect un-propagated garbage accumulated locally in other threads. This will only
/// attempt to collect the accumulated local and global (propagated) garbage.
///
/// # Panic
///
/// If a destructor panics during the garbage collection, theis function will panic aswell.
pub fn try_gc() -> Result<(), ()> {
    // Export the local garbage to ensure that the garbage of the current thread gets collected.
    local::export_garbage();
    // Run the global GC.
    global::try_gc()
}

/// Collect garbage.
///
/// This function does two things:
///
/// 1. Export garbage from current thread to the global queue.
/// 2. Collect all the garbage and run destructors on the unused items.
///
/// If another thread is currently doing 2., it will block until it can be done. This makes it
/// different from `conc::try_gc()`, which will skip the step.
///
/// # Use case
///
/// This is really only neccesary in one case: If you want to ensure that all the destructors of
/// inactive hazards in the current thread are run. If the destructors hold some special logic, you
/// want to execute, this will force the (inactive) ones to run these destructors and thus execute
/// the logic.
///
/// If you just want to reduce memory usage, you will probably be better off with `conc::try_gc()`.
///
/// # Other threads
///
/// This cannot collect un-propagated garbage accumulated locally in other threads. This will only
/// collect the accumulated local and global (propagated) garbage.
///
/// # Panic
///
/// If a destructor panics during the garbage collection, theis function will panic aswell.
pub fn gc() {
    // Export the local garbage to ensure that the garbage of the current thread gets collected.
    local::export_garbage();
    // Try to garbage collect until it succeeds.
    while let Err(()) = global::try_gc() {}
}

/// Declare a pointer unreachable garbage to be deleted eventually.
///
/// This adds `ptr` to the queue of garbage, which eventually will be destroyed through its
/// destructor given in `dtor`. This is ensured to happen at some point _after_ the last guard
/// protecting the pointer is dropped.
///
/// It is legal for `ptr` to be invalidated by `dtor`, such that accessing it is undefined after
/// `dtor` has been run. This means that `dtor` can safely (there are exceptions, see below) run a
/// destructor of `ptr`'s data.
///
/// # Unreachability criterion
///
/// If you invalidate `ptr` in the destructor, it is extremely important that `ptr` is no longer
/// reachable from any data structure: It should be impossible to create _new_ guard representing
/// `ptr` from now on, as such thing can mean that new guards can be created after it is dropped
/// causing use-after-free.
///
/// # Destruction
///
/// If the destructor provided panics under execution, it will cause panic in the garbage
/// collection, and the destructor won't run again.
pub fn add_garbage<T>(ptr: &'static T, dtor: fn(&'static T)) {
    local::add_garbage(unsafe {
        Garbage::new(ptr as *const T as *const u8 as *mut u8, mem::transmute(dtor))
    });
}

/// Add a heap-allocated `Box<T>` as garbage.
///
/// This adds a `Box<T>` represented by pointer `ptr` to the to-be-destroyed garbage queue.
///
/// For more details, see `add_garbage`, which this method is a specialization of.
///
/// # Safety
///
/// This is unsafe as the pointer could be aliased or invalid. To satisfy invariants, the pointer
/// shall be a valid object, allocated through `Box::new(x)` or alike, and shall only be used as
/// long as there are hazard protecting it.
pub fn add_garbage_box<T>(ptr: *const T) {
    local::add_garbage(unsafe {
        Garbage::new_box(ptr)
    });
}
