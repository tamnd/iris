//! Address space reservation and file view mapping on Unix.
//!
//! Three `mmap` calls, distinguished by what they pass for protection and backing. The reservation
//! is anonymous and `PROT_NONE`, a view is the file at `PROT_READ`, and removing a view goes back to
//! anonymous `PROT_NONE`. All three of the last two are `MAP_FIXED` at an address this process
//! already owns, which is the only way to use `MAP_FIXED` that is not a bug: pointed anywhere else
//! it will silently unmap whatever a library happened to have there.

use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, RawFd};
use std::ptr;
use std::ptr::NonNull;

/// Linux will refuse a large `PROT_NONE` reservation under a strict overcommit setting unless it is
/// told the reservation is not going to be backed. The other platforms here either have no such
/// setting or do not define the flag, so it is zero there and the mapping is identical.
#[cfg(target_os = "linux")]
const NORESERVE: libc::c_int = libc::MAP_NORESERVE;
#[cfg(not(target_os = "linux"))]
const NORESERVE: libc::c_int = 0;

/// The alignment a mapping offset has to be a multiple of.
///
/// Windows draws a distinction here that Unix does not: an offset has to land on a sixty four
/// kibibyte boundary there while a length only has to land on a page. Here both are the page size,
/// and the two functions exist so the portable code above can keep the distinction straight rather
/// than picking whichever one happened to work on the machine it was written on.
pub(crate) fn granularity() -> usize {
    page()
}

/// The alignment a mapping length has to be a multiple of.
pub(crate) fn page() -> usize {
    // SAFETY: sysconf reads a static property of the running system. It takes an int, returns a
    // long, and touches no memory this passes it, because this passes it none.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    // A negative answer means sysconf does not know, which cannot happen for _SC_PAGESIZE on any
    // system this builds for. Four kibibytes is the right guess if it somehow does.
    usize::try_from(size).unwrap_or(4096)
}

/// What a view is mapped from. On Unix that is the file itself, so this borrows nothing and holds
/// nothing: the descriptor is kept because `mmap` wants one, and the file outlives it.
pub(crate) struct Backing {
    fd: RawFd,
}

impl Backing {
    /// Reading the descriptor out of a file cannot fail, so neither can this. It returns a `Result`
    /// because the Windows version creates a section object and that very much can, and the portable
    /// code above calls one function rather than two.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the signature is shared with the Windows version"
    )]
    pub(crate) fn new(file: &File) -> io::Result<Self> {
        Ok(Self {
            fd: file.as_raw_fd(),
        })
    }
}

impl fmt::Debug for Backing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backing").field("fd", &self.fd).finish()
    }
}

/// A contiguous run of addresses this process owns and nothing else may be given.
pub(crate) struct Reservation {
    base: NonNull<u8>,
    span: usize,
    /// How much of the reservation currently has a file view over it. The rest is `PROT_NONE`.
    mapped: usize,
}

impl Reservation {
    /// Takes `span` bytes of address space without committing any memory to it.
    ///
    /// `PROT_NONE` and anonymous is a reservation rather than an allocation: nothing is faulted in,
    /// nothing counts against resident memory, and every access to it traps until a view replaces
    /// part of it. That last property is what makes a stale pointer loud rather than silent.
    pub(crate) fn new(span: usize) -> io::Result<Self> {
        // SAFETY: a null address asks the kernel to choose, which is the ordinary use of mmap. The
        // mapping is anonymous, so the file descriptor is unused and -1 is what is passed for it by
        // convention. The result is checked against MAP_FAILED before it is treated as a pointer.
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                span,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(base.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap succeeded and returned a null address"))?;
        Ok(Self {
            base,
            span,
            mapped: 0,
        })
    }

    pub(crate) fn base(&self) -> NonNull<u8> {
        self.base
    }

    pub(crate) fn span(&self) -> usize {
        self.span
    }

    /// Puts `len` bytes of `backing`, starting at `offset`, at the front of the reservation.
    ///
    /// Whatever was there is replaced rather than freed first. `MAP_FIXED` is defined to do that
    /// atomically, so there is no window in which another thread could be handed these addresses.
    pub(crate) fn map(&mut self, backing: &Backing, offset: u64, len: usize) -> io::Result<()> {
        debug_assert!(
            len <= self.span,
            "the caller clamps a view to the reservation"
        );
        let offset = libc::off_t::try_from(offset).map_err(|_| {
            io::Error::other("the mapping offset is past what this platform can express")
        })?;

        // SAFETY: MAP_FIXED at an address inside a reservation this value owns, for no more bytes
        // than that reservation covers, so the only mapping it can replace is one of ours. The
        // result is checked against MAP_FAILED before it is used.
        let got = unsafe {
            libc::mmap(
                self.base.as_ptr().cast(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                backing.fd,
                offset,
            )
        };
        if got == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.mapped = len;
        Ok(())
    }

    /// Takes the view away and leaves the addresses reserved and unreadable.
    ///
    /// This is deliberately not `munmap`. Unmapping would hand the range back to the kernel, and the
    /// kernel is free to give it to the next thread that asks for anything, so the reservation would
    /// stop being contiguous at the first slide under load. Mapping `PROT_NONE` over the range keeps
    /// it and costs the same one call.
    pub(crate) fn unmap(&mut self) -> io::Result<()> {
        if self.mapped == 0 {
            return Ok(());
        }
        // SAFETY: as in map. The address and length are the ones this value mapped, so the mapping
        // being replaced is the one it created.
        let got = unsafe {
            libc::mmap(
                self.base.as_ptr().cast(),
                self.mapped,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | NORESERVE,
                -1,
                0,
            )
        };
        if got == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.mapped = 0;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // One munmap covers the whole reservation whether or not a view is currently over part of
        // it, because a view was mapped inside the same range rather than alongside it.
        //
        // SAFETY: the address and length are the ones this value reserved and has owned since, and
        // nothing else holds them. There is nothing to do about a failure at drop time, and the only
        // way this fails is a length of zero, which the constructor does not produce.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.span);
        }
    }
}

impl fmt::Debug for Reservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reservation")
            .field("base", &self.base)
            .field("span", &self.span)
            .field("mapped", &self.mapped)
            .finish()
    }
}

// SAFETY: the reservation owns its address range outright from construction to drop. Nothing else
// holds the pointer, and every operation that changes the mapping takes &mut self, so moving one
// between threads and sharing a reference to one both follow the ordinary rules.
unsafe impl Send for Reservation {}
// SAFETY: as above. &Reservation permits reading base, span and mapped and nothing else.
unsafe impl Sync for Reservation {}

/// Whether the first and last byte of a range can actually be read, without reading them.
///
/// Asking directly would mean dereferencing an address that is expected to fault, which ends the
/// process rather than answering the question. So this hands the address to the kernel instead:
/// `write` reports an unreadable buffer as `EFAULT` rather than raising a signal, which is the one
/// way to ask this that is defined behaviour and needs no signal handler. The write goes into a pipe
/// that is closed immediately, so at most two bytes go anywhere and they go nowhere.
#[cfg(feature = "probe")]
pub(crate) fn readable(ptr: *const u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    // The last byte matters as much as the first: a view that is shorter than it should be is a
    // mapping bug that a check of the first byte alone reads as success.
    // SAFETY: this only forms the address, it does not read it. The caller is asking about a range
    // it believes is len bytes long, and a one past the end pointer is not formed because the offset
    // is len minus one.
    let last = unsafe { ptr.add(len - 1) };
    probe_one(ptr) && probe_one(last)
}

/// How many file descriptors this process has open, or `None` if it cannot be found out.
///
/// Both directories here list one entry per open descriptor. `/proc/self/fd` is the Linux one and
/// `/dev/fd` is the portable one that also exists there, so the first is tried only because it is
/// the cheaper of the two on the platform that has both.
#[cfg(feature = "probe")]
pub(crate) fn handles() -> Option<u32> {
    for directory in ["/proc/self/fd", "/dev/fd"] {
        if let Ok(entries) = std::fs::read_dir(directory) {
            // Reading the directory holds a descriptor of its own, which is counted here and closed
            // straight after. A caller compares two counts taken the same way, so it cancels.
            return u32::try_from(entries.count()).ok();
        }
    }
    None
}

#[cfg(feature = "probe")]
fn probe_one(ptr: *const u8) -> bool {
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: pipe writes two descriptors into the array it is given, which is two ints long.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        // No pipe means no answer. Reporting "readable" here would turn a resource failure into a
        // silently passing test, so this reports the opposite and the assertion fails loudly.
        return false;
    }

    // SAFETY: writing one byte from an address supplied by the caller. If that address is not
    // readable the kernel returns EFAULT, which is the entire point, and it does not dereference
    // anything on this side of the call.
    let written = unsafe { libc::write(ends[1], ptr.cast(), 1) };
    let error = io::Error::last_os_error();

    // SAFETY: both descriptors came from the pipe call above and are closed exactly once.
    unsafe {
        libc::close(ends[0]);
        libc::close(ends[1]);
    }

    written >= 0 || error.raw_os_error() != Some(libc::EFAULT)
}
