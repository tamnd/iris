//! Range oriented data sources for iris.
//!
//! A decoder declares the byte ranges it needs and the host serves them. That inversion is what lets
//! the same decoder run against a local file, a page cache, and an object store.
//!
//! [`RangeSource`] is that trait. Three implementations come with it: [`MemorySource`] over bytes
//! that are already resident, [`FileSource`] over a local file read through the sliding [`Window`],
//! and `ObjectSource` over an object store, behind the `object-store` feature. They are
//! interchangeable because they all pass the same suite in [`conformance`], which is public so that
//! a fourth implementation written somewhere else can be held to the same promises.
//!
//! [`Segment`] is not a fourth source but an adapter over any of them. It presents a byte range of
//! one source as a source in its own right, which is how a decoder is shown one section of a
//! container and nothing else when the container is too large to hold.
//!
//! The one thing to know before reading the trait is that asking for a range does not wait. It
//! either hands back the bytes or says they are not here yet, and the host does something else in
//! between. That is what a single threaded host needs and it is what the resumable path in the
//! sandbox is built on. See the [`source`] module for the rest.
//!
//! # Unsafe code
//!
//! This is the one crate in the workspace that has any. Reserving address space and mapping a file
//! into part of it is not expressible without it, and the alternative to writing it here is writing
//! it in the crate that runs the sandbox, which is the last place it should be. Every other crate
//! carries `#![forbid(unsafe_code)]` and keeps it.
//!
//! All of it is in [`window`] and its platform modules, every block carries a comment saying why it
//! is sound, and the stress test in `tests/window.rs` runs thousands of remap cycles on all four
//! supported platforms on every change.

pub mod memory;
pub mod segment;
pub mod source;

pub use memory::MemorySource;
pub use segment::Segment;
pub use source::{Fetch, RangeSource, SourceError, bounds, read_blocking};

#[cfg(feature = "conformance")]
pub mod conformance;

#[cfg(feature = "object-store")]
pub mod object;

#[cfg(feature = "object-store")]
pub use object::ObjectSource;

#[cfg(any(unix, windows))]
mod sys;

#[cfg(any(unix, windows))]
pub mod file;

#[cfg(any(unix, windows))]
pub mod window;

#[cfg(any(unix, windows))]
pub use file::FileSource;

#[cfg(any(unix, windows))]
pub use window::{DEFAULT_SPAN, Window, WindowError};

/// Asks the operating system whether an address range can be read, without reading it.
///
/// This exists for the tests that hold an address across a window slide, where the property being
/// checked is that the address stopped being readable and the obvious way to check it ends the
/// process. It is behind a feature because it is a question about a mapping rather than about a data
/// source, and nothing that uses this crate for its actual purpose should need to ask it.
#[cfg(all(feature = "probe", any(unix, windows)))]
pub mod probe {
    /// Whether the first and last byte of `len` bytes at `ptr` can be read.
    ///
    /// False means a read would fault. True means it would not, which is not the same as saying the
    /// bytes are the ones the caller expects.
    ///
    /// On Unix this hands the address to `write` and looks for `EFAULT`, because there is no
    /// portable way to ask what a mapping looks like. On Windows it asks `VirtualQuery` directly.
    /// Neither one dereferences the address.
    #[must_use]
    pub fn readable(ptr: *const u8, len: usize) -> bool {
        crate::sys::readable(ptr, len)
    }

    /// How many operating system handles this process holds, or `None` where that cannot be found
    /// out.
    ///
    /// The number on its own means nothing. The difference between two of them, taken the same way
    /// on either side of a loop, is what a handle leak looks like. `None` means the platform did not
    /// answer, and a caller should skip the check rather than treat it as zero.
    ///
    /// Descriptors on Unix, handles on Windows. Those are not the same kind of object and the counts
    /// are not comparable across platforms, which does not matter, because the only comparison worth
    /// making is against another count from the same process.
    #[must_use]
    pub fn handles() -> Option<u32> {
        crate::sys::handles()
    }
}

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
