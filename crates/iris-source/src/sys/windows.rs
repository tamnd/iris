//! Address space reservation and file view mapping on Windows, via the placeholder API.
//!
//! This is the reason issue #19 exists. On Unix a view is moved with one `mmap`, because `MAP_FIXED`
//! is defined to replace whatever is at an address without ever letting go of it. Windows has no
//! equivalent of that on the older `MapViewOfFileEx` path: a view has to be unmapped before another
//! can be mapped, and the instant it is unmapped the addresses are free for anything in the process
//! to take. On a threaded host that is not a theoretical race, it is the ordinary case.
//!
//! The placeholder API, added in Windows 10 1803, is the answer. A placeholder is a reservation the
//! memory manager will not hand to anybody else and will not fault in either, and a view can be
//! swapped into and out of one without the range ever becoming free. Every call below carries a flag
//! whose whole job is to say "and keep holding the addresses":
//!
//! - `MEM_RESERVE_PLACEHOLDER` takes the range in the first place.
//! - `MEM_PRESERVE_PLACEHOLDER` on `VirtualFree` splits it rather than releasing it.
//! - `MEM_REPLACE_PLACEHOLDER` on `MapViewOfFile3` puts a view into one exactly.
//! - `MEM_PRESERVE_PLACEHOLDER` on `UnmapViewOfFile2` turns the view back into a placeholder.
//! - `MEM_COALESCE_PLACEHOLDERS` on `VirtualFree` rejoins the two halves.
//!
//! Two constraints from that list shape the code and are easy to get wrong. `MEM_REPLACE_PLACEHOLDER`
//! requires the view to be exactly the size of the placeholder it replaces, so the split and the map
//! are always the same number. And a placeholder can only be split and coalesced on page boundaries,
//! while a file offset still has to sit on an allocation granularity boundary, so this module reports
//! those as two separate numbers and the window rounds offsets by one and lengths by the other.

use std::ffi::c_void;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle as _;
use std::ptr;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_EXECUTE,
    PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
};
// The odd one out. Every other placeholder flag is in Win32::System::Memory next to the calls that
// take them, and this one is in SystemServices, which is where the Windows metadata puts constants
// that no signature in the API surface mentions by type. It is the reason this crate takes a feature
// it otherwise has no use for, which is still a better trade than writing the number out here.
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::SystemServices::MEM_COALESCE_PLACEHOLDERS;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

fn system_info() -> SYSTEM_INFO {
    // SAFETY: GetSystemInfo fills the structure it is given and reads nothing from it, and
    // SYSTEM_INFO is integers and a pointer pair, so an all zero value is a valid one to hand it.
    let mut info: SYSTEM_INFO = unsafe { core::mem::zeroed() };
    // SAFETY: the pointer is to a live local of exactly the type the call expects.
    unsafe { GetSystemInfo(&raw mut info) };
    info
}

/// The alignment a mapping offset has to be a multiple of. Sixty four kibibytes on every Windows
/// this runs on, and unrelated to the page size, which is the trap for anybody porting from Unix.
pub(crate) fn granularity() -> usize {
    usize::try_from(system_info().dwAllocationGranularity).unwrap_or(64 * 1024)
}

/// The alignment a mapping length has to be a multiple of, which is where a placeholder may be split.
pub(crate) fn page() -> usize {
    usize::try_from(system_info().dwPageSize).unwrap_or(4096)
}

/// The pseudo handle for this process. It is a constant rather than a real handle, so there is
/// nothing to close and no failure to check.
fn current_process() -> HANDLE {
    // SAFETY: takes nothing, returns a constant, cannot fail.
    unsafe { GetCurrentProcess() }
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// The section object a view is mapped from.
///
/// Unix maps from the file descriptor directly. Windows puts an object in between, and it is a real
/// handle with a real lifetime, so this owns it and closes it. A section created with a maximum size
/// of zero takes the size of the file, which is what is wanted here and also why an empty file has
/// no backing at all: `CreateFileMappingW` refuses a zero length one, and there is nothing to map.
pub(crate) struct Backing {
    section: HANDLE,
    /// How large the section is, which is the size of the file it was made from. A view may not run
    /// past this, so the last view of a file has to be cut back to it.
    len: u64,
}

impl Backing {
    pub(crate) fn new(file: &File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        let handle: HANDLE = file.as_raw_handle().cast();
        // SAFETY: the handle belongs to a file this call borrows for its whole duration. Null
        // security attributes and a null name are the documented way to ask for an unnamed section
        // with default security, and a maximum size of zero asks for the size of the file.
        let section =
            unsafe { CreateFileMappingW(handle, ptr::null(), PAGE_READONLY, 0, 0, ptr::null()) };
        if section.is_null() {
            return Err(last_error());
        }
        Ok(Self { section, len })
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileMappingW above, is closed exactly once, and no view
        // still refers to it because the reservation is dropped before the backing.
        unsafe {
            CloseHandle(self.section);
        }
    }
}

impl fmt::Debug for Backing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backing")
            .field("section", &self.section)
            .field("len", &self.len)
            .finish()
    }
}

/// A contiguous run of addresses this process owns and nothing else may be given.
pub(crate) struct Reservation {
    base: NonNull<u8>,
    span: usize,
    /// How much of the reservation currently holds a view. Zero means the whole span is one
    /// placeholder, which is the state every operation here starts and ends in.
    mapped: usize,
}

impl Reservation {
    pub(crate) fn new(span: usize) -> io::Result<Self> {
        // SAFETY: a null base asks the memory manager to choose. No extended parameters, so the
        // pointer is null and the count is zero. The result is checked before it is used.
        let base = unsafe {
            VirtualAlloc2(
                current_process(),
                ptr::null(),
                span,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                ptr::null_mut(),
                0,
            )
        };
        let base = NonNull::new(base.cast::<u8>()).ok_or_else(last_error)?;
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
    /// Four calls where Unix needs one, and the order is the whole difficulty. Any view already
    /// there goes back to being a placeholder first, then the span is split so there is a
    /// placeholder of exactly `len` to replace, then the view goes in. The addresses are held
    /// throughout: at no point between the first call and the last is this range something another
    /// thread could be handed.
    pub(crate) fn map(&mut self, backing: &Backing, offset: u64, len: usize) -> io::Result<()> {
        debug_assert!(
            len <= self.span,
            "the caller clamps a view to the reservation"
        );
        self.unmap()?;

        if len < self.span {
            // Split the one placeholder into one of len and one of the rest. MEM_RELEASE here does
            // not release anything, because MEM_PRESERVE_PLACEHOLDER is what the pair means, and the
            // two flags have to be passed together.
            //
            // SAFETY: the address is the base of a placeholder this value owns and len is no larger
            // than it. The return value is checked.
            let split = unsafe {
                VirtualFree(
                    self.base.as_ptr().cast::<c_void>(),
                    len,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                )
            };
            if split == 0 {
                return Err(last_error());
            }
        }

        // How many bytes of the section there actually are from here on. A view may not run past the
        // end of the section, and a section is exactly as long as the file, so the last view of a
        // file whose length is not a multiple of the page size asks for more than there is. Windows
        // answers that with ERROR_ACCESS_DENIED, which reads like a permissions problem and is not
        // one. Unix has nothing to say about this, because mmap rounds a length up itself and the
        // bytes in the last page past the end of the file read as zero.
        //
        // The placeholder is still split at the rounded up length and the view still occupies that
        // many pages of address space. It is only the number handed to this call that comes down,
        // which is what keeps MEM_REPLACE_PLACEHOLDER's "exactly the placeholder" rule satisfied:
        // the view rounds back up to the same number of pages.
        let remaining = backing.len.saturating_sub(offset);
        let view_len = usize::try_from(remaining).unwrap_or(usize::MAX).min(len);

        // SAFETY: the base is a placeholder of exactly len bytes, which is what MEM_REPLACE_PLACEHOLDER
        // requires, and the section outlives the view because the caller holds the backing for at
        // least as long as this reservation. No extended parameters.
        let view = unsafe {
            MapViewOfFile3(
                backing.section,
                current_process(),
                self.base.as_ptr().cast::<c_void>(),
                offset,
                view_len,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READONLY,
                ptr::null_mut(),
                0,
            )
        };
        if view.Value.is_null() {
            let failure = last_error();
            // The split placeholder has to be put back together or the next attempt sees a span that
            // is two placeholders where it expects one, and fails for a reason that has nothing to do
            // with what went wrong here.
            if len < self.span {
                self.coalesce();
            }
            return Err(failure);
        }

        self.mapped = len;
        Ok(())
    }

    /// Takes the view away and leaves the addresses reserved and unreadable.
    ///
    /// `MEM_PRESERVE_PLACEHOLDER` is the difference between this and `UnmapViewOfFile`. Without it
    /// the range becomes free memory and the reservation has a hole in it.
    pub(crate) fn unmap(&mut self) -> io::Result<()> {
        if self.mapped == 0 {
            return Ok(());
        }
        let address = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.base.as_ptr().cast::<c_void>(),
        };
        // SAFETY: the address is the base of a view this value mapped and has not unmapped since.
        let ok = unsafe { UnmapViewOfFile2(current_process(), address, MEM_PRESERVE_PLACEHOLDER) };
        if ok == 0 {
            return Err(last_error());
        }
        let was = self.mapped;
        self.mapped = 0;
        if was < self.span {
            self.coalesce();
        }
        Ok(())
    }

    /// Rejoins the two placeholders left by a split back into one covering the whole span.
    ///
    /// A failure here is not reported, and that is deliberate rather than lazy. The only way it
    /// fails is if the range is not two adjacent placeholders, which would mean the state machine
    /// above is already wrong, and the next `map` will fail loudly with the real problem rather than
    /// with a tidy up error that hides it. Nothing is leaked either way: the span is still one
    /// reservation and `Drop` still releases it.
    fn coalesce(&self) {
        // SAFETY: the address is the base of this reservation and the size is its whole span, which
        // is what MEM_COALESCE_PLACEHOLDERS requires. As with the split, MEM_RELEASE releases
        // nothing here, it is half of the flag pair.
        unsafe {
            VirtualFree(
                self.base.as_ptr().cast::<c_void>(),
                self.span,
                MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS,
            );
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // The view first, which also coalesces the span back into one placeholder. MEM_RELEASE will
        // not take a range that is still split, so skipping this leaks the whole reservation, and it
        // leaks it silently: the process keeps running with the address space gone. That is exactly
        // the leak the stress test for this is looking for.
        let _ = self.unmap();

        // SAFETY: the address is the base this value reserved and nothing else holds it. MEM_RELEASE
        // requires a size of zero and the base of the original reservation, which is what is passed.
        unsafe {
            VirtualFree(self.base.as_ptr().cast::<c_void>(), 0, MEM_RELEASE);
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
/// The Unix version of this hands the address to `write` and looks for `EFAULT`, because there is no
/// portable way to ask the kernel what a mapping looks like. Windows has one, so this asks it
/// directly. `VirtualQuery` reports the state and the protection of the region an address falls in,
/// and a placeholder answers `MEM_RESERVE` rather than `MEM_COMMIT`, which is precisely the
/// distinction the window's stale read test is checking for.
#[cfg(feature = "probe")]
pub(crate) fn readable(ptr: *const u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    // SAFETY: this only forms the address, it does not read it. The offset is len minus one, so no
    // one past the end pointer is created.
    let last = unsafe { ptr.add(len - 1) };
    query_readable(ptr) && query_readable(last)
}

/// How many handles this process has open, or `None` if the call refuses.
///
/// This is the number issue #19 is worried about. A section object created once per slide rather
/// than once per window is invisible from anywhere else: the mapping still works, the reads are
/// still correct, and the process quietly accumulates four thousand handles over a stress loop.
#[cfg(feature = "probe")]
pub(crate) fn handles() -> Option<u32> {
    use windows_sys::Win32::System::Threading::GetProcessHandleCount;

    let mut count = 0u32;
    // SAFETY: the pseudo handle for this process is always valid, and the out parameter points at a
    // live local of the right type.
    let ok = unsafe { GetProcessHandleCount(current_process(), &raw mut count) };
    (ok != 0).then_some(count)
}

#[cfg(feature = "probe")]
fn query_readable(ptr: *const u8) -> bool {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};

    // SAFETY: MEMORY_BASIC_INFORMATION is pointers and integers, so an all zero value is valid, and
    // VirtualQuery fills it rather than reading it.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: the address is only inspected, never dereferenced, which is the entire reason this
    // call is used here. The buffer is a live local and the size is its own size.
    let written = unsafe {
        VirtualQuery(
            ptr.cast::<c_void>(),
            &raw mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if written == 0 {
        return false;
    }
    if info.State != MEM_COMMIT {
        return false;
    }
    // PAGE_EXECUTE alone permits execution and nothing else, and a guard page raises on first touch
    // whatever else its protection says. Neither is a page a read succeeds on.
    let unreadable = PAGE_NOACCESS | PAGE_EXECUTE | PAGE_GUARD;
    info.Protect != 0 && info.Protect & unreadable == 0
}
