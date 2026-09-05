//! What the host answers when a decoder asks for bytes it has not been given.
//!
//! The request itself is [`crate::RangeRequest`] when it travels as a record. Across the WebAssembly
//! boundary it is a call rather than a record, because the decoder is stopped inside it waiting for
//! the answer and a record would mean framing a request the caller is going to consume immediately.
//! What crosses is three numbers in and one number out, and the number that comes out is this.
//!
//! The reason it is an integer rather than a boolean is that the four ways a range can fail are not
//! the same failure. Two of them are the decoder asking for the wrong thing and are worth reporting
//! back so it can ask for the right thing instead, and two of them are the host being unable to
//! serve a request that was perfectly reasonable. A decoder that cannot tell those apart has no
//! choice but to give up on all four.

/// The answer to `iris.require_range`.
///
/// Only [`RangeStatus::SERVED`] means the bytes are in the buffer the decoder named. For every other
/// value the buffer has not been written to and holds whatever it held before.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RangeStatus(pub u32);

impl RangeStatus {
    /// The bytes are in the buffer, all of them, exactly as many as were asked for.
    pub const SERVED: Self = Self(0);

    /// The range runs past the end of the source.
    ///
    /// The decoder asked for the wrong thing. This is never a short read, which is the whole reason
    /// it is a status rather than a returned length: a decoder that gets fewer bytes than it asked
    /// for and does not notice produces an answer that is wrong and looks right.
    pub const OUT_OF_BOUNDS: Self = Self(1);

    /// No single request to this host can cover a range that long.
    ///
    /// The host is reading through a window and the range does not fit in one view of it. Asking
    /// again in pieces works, which is what makes this different from the two below.
    pub const TOO_LARGE: Self = Self(2);

    /// The host tried and could not get the bytes.
    ///
    /// A read that failed, a connection that dropped, a source that contradicted itself. Nothing the
    /// decoder does differently will help, and the host already knows the details.
    pub const UNAVAILABLE: Self = Self(3);

    /// This host has nothing to serve ranges from.
    ///
    /// A decoder that pulls ranges was run by a host that handed it the whole source and did not
    /// expect to be asked, or by one that has not attached a source yet. It is a host bug rather
    /// than a decoder bug, and it is separate from [`RangeStatus::UNAVAILABLE`] because the fix is
    /// in a different place.
    pub const NO_SOURCE: Self = Self(4);

    /// Whether the bytes arrived.
    #[must_use]
    pub const fn is_served(self) -> bool {
        self.0 == Self::SERVED.0
    }

    /// The name of this status, if it is one we assigned.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        match self {
            Self::SERVED => Some("served"),
            Self::OUT_OF_BOUNDS => Some("out of bounds"),
            Self::TOO_LARGE => Some("larger than one request can cover"),
            Self::UNAVAILABLE => Some("unavailable"),
            Self::NO_SOURCE => Some("no source attached"),
            _ => None,
        }
    }
}

impl core::fmt::Display for RangeStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "range status {}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Somewhere to print into, because this crate has no allocator and so has no `to_string`.
    struct Buf {
        bytes: [u8; 64],
        len: usize,
    }

    impl Buf {
        const fn new() -> Self {
            Self {
                bytes: [0; 64],
                len: 0,
            }
        }

        fn text(&self) -> &str {
            core::str::from_utf8(&self.bytes[..self.len]).expect("everything written here is utf8")
        }
    }

    impl core::fmt::Write for Buf {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let end = self.len + text.len();
            let room = self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?;
            room.copy_from_slice(text.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    fn printed(status: RangeStatus) -> Buf {
        use core::fmt::Write as _;
        let mut buf = Buf::new();
        write!(&mut buf, "{status}").expect("a status is shorter than the buffer");
        buf
    }

    #[test]
    fn only_zero_means_the_bytes_are_there() {
        assert!(RangeStatus::SERVED.is_served());
        for status in [
            RangeStatus::OUT_OF_BOUNDS,
            RangeStatus::TOO_LARGE,
            RangeStatus::UNAVAILABLE,
            RangeStatus::NO_SOURCE,
        ] {
            assert!(!status.is_served(), "{status} is not the bytes arriving");
        }
    }

    #[test]
    fn a_status_from_a_later_host_still_prints() {
        let future = RangeStatus(9001);
        assert_eq!(future.name(), None);
        assert_eq!(printed(future).text(), "range status 9001");
    }
}
