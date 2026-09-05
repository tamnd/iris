//! The windowed scan, compiled to wasm32 by the toolchain a decoder is written with.
//!
//! The M0 probe measures what the windowed control flow costs by comparing a chunked scan against a
//! flat one. It has always done that with a module written by hand in `wat`, which addresses every
//! load as a base plus an index because that is the natural way to write a chunked loop by hand. The
//! flat loop it is compared against addresses each load with a single register. That difference is
//! not one a real decoder necessarily has, since a decoder is compiled from Rust by a toolchain that
//! gets to choose how to express the same arithmetic, and it may well choose better.
//!
//! So this is the same measurement in the other shape. The two loops here do the same arithmetic
//! over the same bytes as the two loops in the hand written module, and the probe reports an
//! abstraction overhead for both. Where the two disagree, the difference is the cost of the way the
//! probe expresses windowing rather than the cost of windowing, which is a distinction the probe
//! could not make before.
//!
//! # Why the inner loop is a function
//!
//! [`sum_flat`] and [`sum_chunked`] both go through `sum_range`, so the bytes are summed by the same
//! code in both configurations and the only difference between them is the chunk bookkeeping and the
//! host call. If each had its own loop, a compiler that vectorised one and not the other would show
//! up as an abstraction cost, which is exactly the confusion this crate exists to remove.
//!
//! # Unsafe code
//!
//! Importing a function from the host needs an `extern` block, which is the one thing a WebAssembly
//! guest cannot do in safe Rust. Nothing else here is unsafe: the buffer is a `Vec<u8>` and every
//! read of it is an ordinary slice index.
//!
//! The host writes the bytes to be scanned straight into linear memory at the address [`reserve`]
//! hands back, which from inside this module means the contents of that `Vec` change without any
//! code here touching it. That is what a WebAssembly host does and it is benign for a buffer of
//! bytes, but it is the reason [`sum_chunked`] takes the buffer borrow inside the chunk loop rather
//! than around it. Holding a slice across the host call that refills it would be telling the
//! compiler the bytes do not change while they do.

use std::cell::RefCell;

// The host call the chunked scan makes between windows, so that the cost of it is in the
// measurement. The probe binds this to a closure that either does nothing, which is the
// configuration that isolates the control flow, or copies the next window into linear memory, which
// is the configuration that measures a naive refill.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "iris")]
unsafe extern "C" {
    safe fn slide(chunk: i32) -> i32;
}

/// Stands in for the host call when this crate is built for the machine it is developed on.
///
/// Nothing measures the result of a native build. The stub is here so that `cargo fmt`, `cargo
/// clippy` and the workspace test run cover this crate like every other one, which they cannot do if
/// linking it needs a WebAssembly host to resolve an import.
#[cfg(not(target_arch = "wasm32"))]
fn slide(_chunk: i32) -> i32 {
    0
}

thread_local! {
    /// The bytes to scan. Filled by the host, never written by this module.
    static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Sums `bytes` eight at a time, little endian, wrapping.
///
/// The trailing bytes of a range that is not a multiple of eight are dropped. Every range the probe
/// asks for is a multiple of eight, and a scan that quietly read a different number of bytes in one
/// configuration than another would invalidate the comparison, so the probe checks that all
/// configurations produce the same sum rather than trusting this.
fn sum_range(bytes: &[u8]) -> i64 {
    let (words, _) = bytes.as_chunks::<8>();
    let mut acc: u64 = 0;
    for word in words {
        acc = acc.wrapping_add(u64::from_le_bytes(*word));
    }
    acc.cast_signed()
}

/// Makes room for `len` bytes and hands back the address they start at.
///
/// The host needs an address because it writes the bytes to be scanned directly into linear memory.
/// Zero comes back if the request does not fit, which the probe reads as a failure rather than as an
/// address.
#[unsafe(no_mangle)]
pub extern "C" fn reserve(len: i32) -> i32 {
    let Ok(len) = usize::try_from(len) else {
        return 0;
    };
    BUFFER.with_borrow_mut(|buffer| {
        buffer.clear();
        buffer.resize(len, 0);
        i32::try_from(buffer.as_ptr() as usize).unwrap_or(0)
    })
}

/// Sums the first `len` bytes in one pass.
///
/// This is what mapping the whole dataset into guest memory looks like from inside a decode loop,
/// and it is the denominator the windowed shape is measured against.
#[unsafe(no_mangle)]
pub extern "C" fn sum_flat(len: i32) -> i64 {
    let Ok(len) = usize::try_from(len) else {
        return 0;
    };
    BUFFER.with_borrow(|buffer| sum_range(&buffer[..len.min(buffer.len())]))
}

/// Sums `chunks` windows of `win` bytes, advancing by `stride` and calling the host between each.
///
/// With `stride` equal to `win` this walks forward through the whole buffer, which is the shape that
/// isolates what the windowed control flow costs. With `stride` at zero it reads the same window
/// every time, which is the shape where the host has to refill that window between chunks.
#[unsafe(no_mangle)]
pub extern "C" fn sum_chunked(chunks: i32, win: i32, stride: i32) -> i64 {
    let (Ok(chunks), Ok(win), Ok(stride)) = (
        usize::try_from(chunks),
        usize::try_from(win),
        usize::try_from(stride),
    ) else {
        return 0;
    };

    let mut acc: i64 = 0;
    for chunk in 0..chunks {
        let told = slide(i32::try_from(chunk).unwrap_or(0));
        std::hint::black_box(told);

        // Borrowed per chunk rather than around the loop, for the reason in the module documentation:
        // the host call above is allowed to have rewritten these bytes.
        let start = chunk * stride;
        acc = acc.wrapping_add(BUFFER.with_borrow(|buffer| {
            let end = start.saturating_add(win).min(buffer.len());
            if start >= end {
                return 0;
            }
            sum_range(&buffer[start..end])
        }));
    }
    acc
}
