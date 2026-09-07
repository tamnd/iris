//! One `BtrBlocks` decode kernel, built for both sides of the sandbox.
//!
//! The M5 vectorisation question is whether the WebAssembly sandbox costs less on arm64 than on
//! x86-64, and the reason to expect that it does is arithmetic rather than a hunch. WebAssembly's
//! vector width is capped at 128 bits and is going to stay there. Arm Neon is 128 bits. AVX2 is 256.
//! So on x86-64 a native decoder has vectors twice the width the guest can ever have, and on arm64
//! it has the same width, and if the gap between guest and host is mostly vector width then it
//! should be much smaller on arm64.
//!
//! Testing that needs the same decoder on both sides, which is what this crate is. It is built to
//! wasm32 as a `cdylib` and run under Wasmtime, and linked into the probe as an `rlib` and called
//! directly, and [`decode_part`] is the function in both cases. Anything the two builds disagree
//! about is the compiler and the machine, which is the thing being measured.
//!
//! # Why a checksum rather than the values
//!
//! The guest returns one `u64` and the host compares it against its own. Returning the values would
//! mean copying a decoded column out of linear memory on every iteration, and that copy is not
//! decoding: it would land in the guest's number and not in the host's, in proportion to how much
//! data came out, and the ratio would then be partly a measurement of `memcpy`.
//!
//! The checksum is order dependent and value dependent, so it catches a decoder that produced the
//! right number of wrong values as well as one that produced the wrong number. It is not a hash and
//! is not meant to resist anything. What it has to do is fail if the two sides disagree, and there
//! is nothing adversarial on either side of this comparison.
//!
//! # Why the checksum only looks at some of the values
//!
//! [`STRIDE`] values are decoded for every one that is folded in. The first version of this folded
//! every value, and the probe then reported an uncompressed column, which decodes by copying, as
//! taking the same time as a bit packed one, which decodes by shifting and masking. That is because
//! the fold had become the loop: it is a rotate, an add and a multiply per value, each one waiting
//! on the last, so it costs a few cycles per value and a decoder that is doing well costs less than
//! that.
//!
//! A serial dependency chain vectorises on no architecture, so measuring one and calling the answer
//! a vector width result would have been wrong in a way that looked like a finding. Sampling puts
//! the fold at roughly one percent of the loop and leaves the decoders as the thing being timed.
//! What is lost is that the checksum no longer sees every value, which does not matter for what it
//! is for: the two sides run identical source, and whether the decoders are correct at all is what
//! the `iris-btr` conformance suite answers, byte for byte, against the reference.
//!
//! # Unsafe code
//!
//! Two functions, both wasm32 only, both there because a WebAssembly host has no way to hand bytes
//! to a guest except by writing them into linear memory at an address the guest gave it.
//! `guest::reserve` allocates a buffer and returns that address, and `guest::decode` reads it back.
//! Neither is a link, because the module holding them is compiled away on every target that is not
//! wasm32 and rustdoc is not being run for wasm32. Nothing else in this crate is unsafe and the
//! native side reaches none of it.

use iris_btr::{Column, Part};

/// One value in this many is folded into the checksum.
///
/// A power of two, and large enough that the fold is not the loop. Sampling still notices a value
/// that changed and still notices values that were reordered, because a permutation moves different
/// values into the positions that are looked at.
pub const STRIDE: usize = 64;

/// Decodes one column part and folds a sample of the values into a checksum.
///
/// This is the whole measured kernel. Parsing the part is included because a decoder that is handed
/// bytes has to parse them, and it is a fixed cost that lands identically on both sides.
///
/// # Errors
///
/// Returns `None` if the bytes are not a part this crate can read, which for the conformance corpus
/// the probe runs against cannot happen and is not a case worth reporting in detail here. The probe
/// treats it as a fixture problem and says so.
#[must_use]
pub fn decode_part(bytes: &[u8]) -> Option<u64> {
    let part = Part::parse(bytes).ok()?;
    // Seeded rather than started at zero, so that zero can mean failure and nothing else. The guest
    // side has no way to return an error across the export boundary and returns zero instead, and a
    // part that legitimately folded to zero would then be indistinguishable from a decoder that
    // refused. An all null column does exactly that from a zero seed, so this is not hypothetical.
    // The seed is the 64 bit FNV offset basis, picked to go with the multiplier below.
    let mut sum = 0xcbf2_9ce4_8422_2325_u64;
    for index in 0..part.chunks() {
        let chunk = part.chunk(index).ok()?;
        match chunk.decode().ok()? {
            // Sign extended before it is widened, so that a negative value and its unsigned
            // reinterpretation do not fold to the same thing.
            Column::Integer(values) => {
                sum = sample(
                    sum,
                    values.len(),
                    values.iter().map(|v| i64::from(*v).cast_unsigned()),
                );
            }
            // By bit pattern rather than by value, because a decoder that returned a different NaN
            // payload from the reference has still returned something different, and comparing
            // doubles by value would not notice.
            Column::Double(values) => {
                sum = sample(sum, values.len(), values.iter().map(|v| v.to_bits()));
            }
            // The offsets as well as the bytes. Two columns can hold the same bytes split into
            // different rows, and a checksum that only saw the bytes would call those equal.
            Column::Text(strings) => {
                let offsets = strings.offsets();
                let bytes = strings.bytes();
                sum = sample(sum, offsets.len(), offsets.iter().map(|o| u64::from(*o)));
                sum = sample(sum, bytes.len(), bytes.iter().map(|b| u64::from(*b)));
            }
        }
    }
    Some(sum)
}

/// Folds the length of a run of values and every [`STRIDE`] th value into the checksum.
///
/// The length goes in as well as the sample, because a decoder that returned half as many values
/// would otherwise have to be caught by the sample landing differently, and the length catches it
/// directly and costs nothing.
fn sample(sum: u64, len: usize, values: impl Iterator<Item = u64>) -> u64 {
    let mut sum = mix(sum, u64::try_from(len).unwrap_or(u64::MAX));
    for value in values.step_by(STRIDE) {
        sum = mix(sum, value);
    }
    sum
}

/// One step of the fold.
///
/// A multiply and a rotate, which is enough to make the fold order dependent. It is a serial
/// dependency chain and deliberately not made into anything faster, because what keeps it out of the
/// measurement is how rarely it runs rather than how quick it is. The constant is the 64 bit `FNV`
/// prime, chosen because it is a well known odd multiplier and not because anything here needs its
/// properties.
const fn mix(sum: u64, value: u64) -> u64 {
    sum.rotate_left(7)
        .wrapping_add(value)
        .wrapping_mul(0x0000_0100_0000_01b3)
}

/// The guest side.
///
/// Two exports and a buffer between them, which is the smallest interface that lets a host put bytes
/// in front of a guest. There is no `iris` ABI here on purpose: this probe is about what the
/// sandbox costs a decode loop, and routing the bytes through range requests would fold the host
/// call cost into it, which M0 already measured on its own and which would confound this.
///
/// Public because the two functions in it are `#[unsafe(no_mangle)]` exports and a module that hides
/// them makes the compiler say they are unreachable, which they are from Rust and are not from the
/// host that is about to call them.
#[cfg(target_arch = "wasm32")]
pub mod guest {
    use std::cell::RefCell;

    thread_local! {
        /// The part the host most recently wrote, kept alive between the two calls.
        static PART: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// Makes room for `len` bytes and returns where the host should write them.
    ///
    /// The pointer is valid until the next call to this function, which is the only thing that can
    /// move the buffer. The host writes the bytes and then calls [`decode`], and nothing else runs
    /// in between because a WebAssembly guest has no threads unless it was given some.
    #[unsafe(no_mangle)]
    pub extern "C" fn reserve(len: u32) -> u32 {
        PART.with_borrow_mut(|part| {
            part.clear();
            part.resize(len as usize, 0);
            // SAFETY: the pointer comes from a live `Vec` held in a thread local that outlives this
            // call, and it is only ever handed back to the host as an offset into linear memory. A
            // wasm32 pointer is 32 bits, so the cast cannot lose anything.
            part.as_mut_ptr() as u32
        })
    }

    /// Decodes what the host wrote and returns the checksum.
    ///
    /// Returns zero if the bytes are not a part. A successful decode starts from a non zero seed, so
    /// zero is a value the fold does not otherwise reach and the host reads it as a refusal.
    #[unsafe(no_mangle)]
    pub extern "C" fn decode() -> u64 {
        PART.with_borrow(|part| super::decode_part(part).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_part, mix};

    #[test]
    fn bytes_that_are_not_a_part_decode_to_nothing_rather_than_panicking() {
        assert_eq!(decode_part(&[]), None);
        assert_eq!(decode_part(&[0xff; 32]), None);
    }

    #[test]
    fn the_fold_notices_the_order_the_values_came_in() {
        let forwards = [1u64, 2, 3].iter().fold(0, |sum, &v| mix(sum, v));
        let backwards = [3u64, 2, 1].iter().fold(0, |sum, &v| mix(sum, v));
        assert_ne!(forwards, backwards);
    }

    #[test]
    fn the_fold_notices_a_value_that_changed() {
        assert_ne!(mix(0, 1), mix(0, 2));
    }
}
