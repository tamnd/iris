//! A file view that slides inside a fixed reservation of address space.
//!
//! # What this is for
//!
//! A decoder in the sandbox asks for byte ranges. The host has to put those bytes at an address the
//! decoder can read, and the obvious way to do that is to map the whole file, which is what the
//! prior art does. Mapping the whole file is fast and it costs address space proportional to the
//! dataset, which is fine until the dataset is larger than the address space or until there are a
//! thousand of them open at once.
//!
//! A window is the other trade. It reserves a fixed span of addresses once, maps a piece of the file
//! into it, and moves that piece when a request falls outside it. The address space cost is constant
//! and chosen by the host rather than by the data. The cost is that a request which does not fall in
//! the current view pays for a remap, so the win depends entirely on requests being clustered, which
//! for a columnar scan they are.
//!
//! # The property that matters
//!
//! When the view moves, everything the old view pointed at has to stop being readable. Not stop
//! being correct, stop being readable. A decoder that reads through a stale pointer and gets bytes
//! from the part of the file that used to be there produces an answer that is wrong and looks right,
//! and there is nothing downstream that can catch it. So the vacated range does not become free
//! memory and does not become zeroes: it goes back to being reserved and unreadable, and a read
//! through a stale pointer faults.
//!
//! Rust's own rules cover the safe path already, because [`Window::range`] borrows the window
//! mutably and hands back a slice tied to that borrow, so a slice cannot outlive the view it came
//! from. That is not the case the gate is about. The case the gate is about is a raw address that
//! crossed into a sandbox, where the borrow checker is not present, and the test for it is in
//! `tests/window.rs`.
//!
//! # Alignment
//!
//! Offsets and lengths are rounded by two different numbers, which is a Windows distinction that
//! Unix does not have and is the easiest thing here to get wrong on a machine where they happen to
//! be equal. A mapping offset has to be a multiple of the allocation granularity, sixty four
//! kibibytes on Windows and the page size everywhere else. A mapping length has to be a multiple of
//! the page size. Rounding a length up by the allocation granularity instead looks correct on Unix
//! and fails on Windows at the end of a file, because the pages past the end of the section are not
//! part of it.
//!
//! A view is therefore rounded out at both ends and is usually larger than what was asked for. The
//! slice handed back is not: it is exactly the requested range, cut out of the middle.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::sys;

/// How much address space a window reserves when the caller does not say.
///
/// Large enough that a column chunk in a scan lands inside it and no remap happens, small enough
/// that a host can hold many of them. It is a default rather than a tuned number, because the
/// measurement that would tune it needs a real scan and there is not one yet.
pub const DEFAULT_SPAN: usize = 4 * 1024 * 1024;

/// What can go wrong when opening a window or moving it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WindowError {
    /// The operating system refused a reservation, a mapping or a file operation.
    #[error("{operation} failed: {source}")]
    Os {
        /// Which step failed, so the error says where in the sequence it stopped.
        operation: &'static str,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },

    /// The requested range runs past the end of the file.
    #[error("bytes {at}..{end} were asked for and the file is {len} bytes long")]
    OutOfBounds {
        /// Where the request started.
        at: u64,
        /// One past where it ended.
        end: u64,
        /// How long the file is.
        len: u64,
    },

    /// The request cannot be covered by one view, so this window cannot serve it at all.
    ///
    /// This is a configuration error rather than a data error: the window was opened with a span
    /// smaller than the largest range it was going to be asked for.
    ///
    /// `wanted` can be larger than the length that was requested, and the difference is not a
    /// mistake. A view has to start on an alignment boundary, so a request that starts part of the
    /// way into one needs the bytes before it mapped as well, and `wanted` is what the mapping has
    /// to cover rather than what the caller asked to read.
    #[error("covering that range needs {wanted} bytes in one view and this window reserved {span}")]
    TooLarge {
        /// How many bytes a single view would have to cover, including alignment.
        wanted: usize,
        /// How many the window can map at once.
        span: usize,
    },
}

type Result<T> = std::result::Result<T, WindowError>;

fn os(operation: &'static str) -> impl FnOnce(io::Error) -> WindowError {
    move |source| WindowError::Os { operation, source }
}

/// Where the current view sits. Both fields are already rounded to the platform's alignments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct View {
    at: u64,
    len: u64,
}

impl View {
    /// Whether the half open range `at..at + len` is entirely inside this view.
    fn covers(self, at: u64, len: u64) -> bool {
        let Some(want_end) = at.checked_add(len) else {
            return false;
        };
        let Some(have_end) = self.at.checked_add(self.len) else {
            return false;
        };
        at >= self.at && want_end <= have_end
    }
}

/// A read only view of part of a file, at a fixed address, that can be moved.
///
/// See the [module documentation](self) for what this is for and what it guarantees. In short: the
/// address the window lives at is reserved once and held until the window is dropped, and moving the
/// view makes the bytes that used to be there unreadable rather than stale.
pub struct Window {
    // Declaration order is drop order, and these three have to come apart in this order. The
    // reservation unmaps the view, which has to happen before the section it was mapped from is
    // closed, which has to happen before the file underneath it is. Both platforms tolerate the
    // other order, because a mapping keeps its own reference to what it maps, but relying on that
    // means the code is correct for a reason that is not visible where the code is.
    reservation: sys::Reservation,
    /// None for an empty file, which has nothing to map and no section to map it from.
    backing: Option<sys::Backing>,
    /// Held because the mapping refers to it. On Unix the mapping was made from a descriptor number
    /// rather than from a reference, so dropping the file early would close it out from under the
    /// next slide.
    file: File,
    len: u64,
    view: Option<View>,
    slides: u64,
}

impl Window {
    /// Opens `path` read only and reserves [`DEFAULT_SPAN`] bytes of address space for it.
    ///
    /// # Errors
    ///
    /// If the file cannot be opened or measured, or the reservation is refused.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(os("opening the file"))?;
        Self::with_span(file, DEFAULT_SPAN)
    }

    /// Reserves `span` bytes of address space for `file`, rounded up to what the platform allows.
    ///
    /// The span bounds the largest range this window can serve in one piece, so it has to be at
    /// least as large as the largest request plus the alignment slack in front of it. A view starts
    /// on an allocation boundary, so in the worst case that slack is one whole unit of allocation
    /// granularity, which is sixty four kibibytes on Windows and the page size elsewhere. A span of
    /// exactly one unit therefore serves a request only when it happens not to straddle a boundary,
    /// and a span for a largest request of `n` wants to be at least `n` plus one unit.
    ///
    /// A span of zero is rounded up to one unit of allocation granularity rather than rejected,
    /// because a window over an empty file is a reasonable thing to ask for and reserving nothing is
    /// not.
    ///
    /// # Errors
    ///
    /// If the file cannot be measured, or the reservation is refused.
    pub fn with_span(file: File, span: usize) -> Result<Self> {
        let len = file.metadata().map_err(os("measuring the file"))?.len();

        let unit = sys::granularity();
        let span = round_up(span.max(1), unit).ok_or(WindowError::TooLarge {
            wanted: span,
            span: usize::MAX,
        })?;

        let reservation = sys::Reservation::new(span).map_err(os("reserving address space"))?;

        // An empty file has no section to map from. Windows refuses to create one and Unix would
        // accept it and produce a mapping nothing may read, so neither platform gains anything from
        // having one, and every path below already handles a window with no view.
        let backing = if len == 0 {
            None
        } else {
            Some(sys::Backing::new(&file).map_err(os("preparing the file for mapping"))?)
        };

        Ok(Self {
            reservation,
            backing,
            file,
            len,
            view: None,
            slides: 0,
        })
    }

    /// How long the file is.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The file this window reads from.
    #[must_use]
    pub fn file(&self) -> &File {
        &self.file
    }

    /// How much address space this window holds.
    #[must_use]
    pub fn span(&self) -> usize {
        self.reservation.span()
    }

    /// How many times the view has moved since the window was opened.
    ///
    /// A scan whose requests are clustered slides rarely. One that slides on nearly every request is
    /// either reading in an order the window is the wrong structure for or was opened with too small
    /// a span, and this is how a host tells those apart from the outside.
    #[must_use]
    pub fn slides(&self) -> u64 {
        self.slides
    }

    /// The bytes of the file from `at`, `len` of them.
    ///
    /// If the range is already inside the current view this costs a comparison. If it is not, the
    /// view moves first, which unmaps the old one and maps a new one, and every address the old view
    /// covered stops being readable.
    ///
    /// The returned slice borrows the window, so it cannot outlive the view it came from. That is
    /// the safe half of the guarantee in the [module documentation](self); the other half is about
    /// raw addresses and is a property of the mapping rather than of this signature.
    ///
    /// # Errors
    ///
    /// [`WindowError::OutOfBounds`] if the range runs past the end of the file,
    /// [`WindowError::TooLarge`] if no single view could cover it, and [`WindowError::Os`] if the
    /// remap is refused.
    pub fn range(&mut self, at: u64, len: usize) -> Result<&[u8]> {
        let wide = len as u64;
        let end = at.saturating_add(wide);
        if end > self.len {
            return Err(WindowError::OutOfBounds {
                at,
                end,
                len: self.len,
            });
        }

        // What a view has to cover is the request plus however far into an alignment unit it starts,
        // because a view cannot start anywhere else. A request of exactly the span therefore does
        // not fit unless it happens to be aligned, which is worth an error that says so rather than
        // a mapping that quietly comes up short.
        let unit = sys::granularity() as u64;
        let skew = usize::try_from(at % unit).unwrap_or(usize::MAX);
        let needed = skew.saturating_add(len);
        if needed > self.span() {
            return Err(WindowError::TooLarge {
                wanted: needed,
                span: self.span(),
            });
        }

        if !self.view.is_some_and(|view| view.covers(at, wide)) {
            self.slide_to(at)?;
        }

        let Some(view) = self.view else {
            // No view means an empty file, which the bounds check only lets through for a zero
            // length request. There is nothing mapped and nothing to hand back.
            debug_assert_eq!(len, 0);
            return Ok(&[]);
        };

        // The view starts at or before `at` by construction, so this does not go negative, and it is
        // smaller than the span, so it fits.
        let offset = usize::try_from(at - view.at).unwrap_or(0);

        // SAFETY: the view is mapped and readable across view.at..view.at + view.len, which the
        // check above established contains at..at + len, so offset..offset + len is inside one
        // mapping. The slice borrows self for its lifetime and nothing can move the view without
        // &mut self, so the mapping outlives the slice.
        let bytes = unsafe {
            std::slice::from_raw_parts(self.reservation.base().as_ptr().add(offset), len)
        };
        Ok(bytes)
    }

    /// Moves the view so that it starts at the alignment boundary at or below `at`.
    ///
    /// The view runs from there for as much of the span as the file has left, rounded up to a page
    /// so that the last view of a file covers the tail. Starting the view at the request rather than
    /// centred on it is deliberate: a scan reads forwards, so the bytes worth having mapped are the
    /// ones after the request and not the ones before it.
    ///
    /// The caller has already established that the request fits, so this does not check again.
    fn slide_to(&mut self, at: u64) -> Result<()> {
        let Some(backing) = self.backing.as_ref() else {
            return Ok(());
        };

        let unit = sys::granularity() as u64;
        let start = at - (at % unit);
        let remaining = self.len - start;

        let span = self.reservation.span();
        let available = usize::try_from(remaining).unwrap_or(usize::MAX);
        // Round up to a page so the last view reaches the end of the file. The bytes between the end
        // of the file and the end of that page read as zero on both platforms and are never inside a
        // slice this hands out, because every slice is bounded by the file length.
        let view_len = round_up(available.min(span), sys::page())
            .unwrap_or(span)
            .min(span);

        // A zero length request at exactly the end of the file has nothing after it to map, and a
        // mapping of no bytes is refused by both platforms with an error that says nothing useful.
        // The window is left with no view, which is the state it is already in for an empty file and
        // which the caller path below already handles by handing back an empty slice.
        //
        // The fuzzer found this in the first minute it ran, on the input `range(len, 0)`, which is
        // the shape a decoder produces when it asks for a column that happens to be empty and sits
        // last in the file. Nothing about it looks like an edge case from inside the arithmetic.
        if view_len == 0 {
            self.view = None;
            return self
                .reservation
                .unmap()
                .map_err(os("unmapping the previous view"));
        }

        // The old view goes first even though Unix could replace it in one call, so that a failed
        // map leaves the window with nothing mapped rather than with a view it thinks has moved.
        // Windows has to do it in this order anyway, and a state machine with one shape on both
        // platforms is worth one extra call on the platform that could have skipped it.
        self.view = None;
        self.reservation
            .unmap()
            .map_err(os("unmapping the previous view"))?;
        self.reservation
            .map(backing, start, view_len)
            .map_err(os("mapping the file into the reservation"))?;

        self.view = Some(View {
            at: start,
            len: view_len as u64,
        });
        self.slides += 1;
        Ok(())
    }

    /// The address the reservation starts at.
    ///
    /// This is the address a host would hand to a sandbox, and it does not move for the life of the
    /// window, which is the point of reserving rather than mapping. Reading through it is only
    /// defined for the part of the reservation the current view covers. It is public so that the
    /// stale read tests can hold an address across a slide and check that it stopped being readable,
    /// which is a property no safe signature can express.
    #[must_use]
    pub fn address(&self) -> *const u8 {
        self.reservation.base().as_ptr()
    }

    /// Where the current view sits in the file and how long it is, or `None` if nothing is mapped.
    ///
    /// The length is the mapped length, which is rounded out to a page and so is usually longer than
    /// the part of the file it covers.
    #[must_use]
    pub fn mapped(&self) -> Option<(u64, usize)> {
        self.view
            .map(|view| (view.at, usize::try_from(view.len).unwrap_or(usize::MAX)))
    }
}

impl fmt::Debug for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The reservation, the section and the file are left out on purpose. Their debug output is
        // a raw address and two handle numbers, which say nothing a reader can use and change on
        // every run, and this is the type somebody prints when a slide went wrong.
        f.debug_struct("Window")
            .field("len", &self.len)
            .field("span", &self.span())
            .field("view", &self.view)
            .field("slides", &self.slides)
            .finish_non_exhaustive()
    }
}

/// `value` rounded up to the next multiple of `unit`, or `None` if that overflows.
fn round_up(value: usize, unit: usize) -> Option<usize> {
    debug_assert!(
        unit != 0,
        "an alignment of zero is not something a platform reports"
    );
    let over = value % unit;
    if over == 0 {
        return Some(value);
    }
    value.checked_add(unit - over)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_up_leaves_exact_multiples_alone() {
        assert_eq!(round_up(0, 4096), Some(0));
        assert_eq!(round_up(4096, 4096), Some(4096));
        assert_eq!(round_up(1, 4096), Some(4096));
        assert_eq!(round_up(4097, 4096), Some(8192));
        assert_eq!(round_up(usize::MAX, 4096), None);
    }

    #[test]
    fn a_view_covers_a_range_inside_it_and_nothing_else() {
        let view = View { at: 100, len: 50 };
        assert!(view.covers(100, 50));
        assert!(view.covers(120, 10));
        assert!(view.covers(150, 0));
        assert!(!view.covers(99, 1));
        assert!(!view.covers(150, 1));
        assert!(!view.covers(140, 11));
        // A length that would overflow the addition is not covered by anything.
        assert!(!view.covers(u64::MAX, 2));
    }
}
