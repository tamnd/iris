//! The two implementations of reserving address space and mapping a file into part of it.
//!
//! Both platforms are asked for the same three things, and the reason this module exists is that
//! they answer in shapes that have almost nothing in common.
//!
//! The operation the window needs is not "map a file". It is "hold a contiguous range of addresses
//! that nothing else may take, and move a file view around inside it". Holding the range is the hard
//! part. If a slide unmapped the old view and then mapped the new one, there would be a moment with
//! a hole in the middle of the reservation, and on a threaded host that hole is something another
//! allocation can land in. The next slide then either fails or, worse, succeeds somewhere else, and
//! the window is no longer contiguous. Both platforms here are written so that no such moment
//! exists: the address range is owned from the first reservation until `Drop`, and mapping is always
//! a replacement rather than a free followed by an allocate.
//!
//! Unix gets this from `MAP_FIXED`, which replaces whatever is at an address atomically, so a view
//! is removed by mapping `PROT_NONE` over it rather than by calling `munmap`. Windows gets it from
//! the placeholder API, where a reservation is explicitly split, the view replaces one half, and
//! removing the view turns it back into a placeholder rather than into free space. The Windows
//! sequence is four calls where the Unix one is one, which is exactly why the gate for this is a
//! stress test rather than a unit test.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{Backing, Reservation, granularity, page};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{Backing, Reservation, granularity, page};

#[cfg(feature = "probe")]
#[cfg(unix)]
pub(crate) use unix::{handles, readable};

#[cfg(feature = "probe")]
#[cfg(windows)]
pub(crate) use windows::{handles, readable};
