//! A source over a local file, read through a sliding window.
//!
//! This is [`Window`] with the trait bolted on, and the reason it is a thin wrapper rather than a
//! second implementation is that the window already does the hard part. What this adds is the
//! bounded promise: a window can serve a range only if the range fits inside one view, and
//! [`RangeSource::largest`] is where a host finds out how large that is before it asks.
//!
//! Nothing here is ever [`Fetch::Pending`]. A page fault on a mapped file does block the thread,
//! which means this source is not the one that makes a single threaded host interesting, and saying
//! so is more useful than pretending a mapping is asynchronous. What it is instead is the fastest
//! path for data that is already in the page cache, which is the case the M4 gate measures.

use std::fs::File;
use std::path::Path;

use crate::source::{Fetch, RangeSource, SourceError};
use crate::sys;
use crate::window::{Window, WindowError};

/// A [`RangeSource`] over a local file, backed by a window of fixed address space.
#[derive(Debug)]
pub struct FileSource {
    window: Window,
}

impl FileSource {
    /// Opens `path` read only, with a window of [`crate::DEFAULT_SPAN`] bytes.
    ///
    /// # Errors
    ///
    /// If the file cannot be opened or measured, or the address space reservation is refused.
    pub fn open(path: &Path) -> Result<Self, SourceError> {
        Ok(Self {
            window: Window::open(path)?,
        })
    }

    /// Opens `file` with a window of `span` bytes, rounded up to what the platform allows.
    ///
    /// The span bounds what a single range can be. See [`RangeSource::largest`] for the number a
    /// host should actually size its requests against, which is smaller than the span by the worst
    /// case alignment slack.
    ///
    /// # Errors
    ///
    /// If the file cannot be measured, or the reservation is refused.
    pub fn with_span(file: File, span: usize) -> Result<Self, SourceError> {
        Ok(Self {
            window: Window::with_span(file, span)?,
        })
    }

    /// How many times the view has moved since the file was opened.
    ///
    /// The number a scan is judged by. Requests that are clustered slide rarely, and a scan that
    /// slides on nearly every request is either reading in an order a window is the wrong structure
    /// for or was given too small a span. See [`Window::slides`].
    #[must_use]
    pub fn slides(&self) -> u64 {
        self.window.slides()
    }

    /// How much address space this source holds.
    #[must_use]
    pub fn span(&self) -> usize {
        self.window.span()
    }

    /// The window underneath, for a host that wants the mapping details rather than the bytes.
    #[must_use]
    pub fn window(&self) -> &Window {
        &self.window
    }
}

impl RangeSource for FileSource {
    fn len(&self) -> u64 {
        self.window.len()
    }

    fn largest(&self) -> Option<usize> {
        // A view has to start on an allocation boundary, so a request that starts one byte past one
        // needs that whole unit mapped in front of it. Subtracting a full unit from the span is
        // therefore the length that is served wherever it starts, which is the promise this method
        // makes. The best case is a whole span and a host that sizes against the best case gets
        // TooLarge on the requests that happen to be misaligned, which is the worst kind of bug to
        // find in production.
        Some(self.span().saturating_sub(sys::granularity()))
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        Ok(Fetch::Ready(self.window.range(at, len)?))
    }
}

impl From<WindowError> for SourceError {
    fn from(error: WindowError) -> Self {
        match error {
            WindowError::Os { operation, source } => SourceError::Io { operation, source },
            WindowError::OutOfBounds { at, end, len } => SourceError::OutOfBounds { at, end, len },
            WindowError::TooLarge { wanted, span } => SourceError::TooLarge {
                wanted,
                largest: span,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_promised_length_leaves_room_for_the_worst_alignment() {
        let file = tempfile::tempfile().expect("a temporary file");
        let source = FileSource::with_span(file, 4 * 1024 * 1024).expect("a window");

        let largest = source.largest().expect("a windowed source is bounded");
        assert_eq!(largest, source.span() - sys::granularity());

        // Whatever offset a range of that length starts at, it fits in one view.
        assert!(largest + (sys::granularity() - 1) < source.span());
    }
}
