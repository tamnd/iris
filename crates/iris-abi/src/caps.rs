//! Capabilities, and the set of them each side carries.
//!
//! A capability is one bit meaning "this side can do this thing". The host says what it offers, the
//! decoder says what it requires, and if the decoder requires something the host does not offer
//! then the two of them stop, on purpose, with a message that says which bit was the problem.
//!
//! The point of naming capabilities rather than bumping a version number for each one is that
//! capabilities compose. A decoder that needs sliding windows and a decoder that needs filter
//! pushdown are not ordered relative to each other, and pretending they are by giving them version
//! numbers means every host has to implement every feature in order to claim the number.

use crate::wire::Reader;

/// One capability, identified by its bit position.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Capability(pub u16);

impl Capability {
    /// The decoder pulls bytes of the source by asking for ranges, rather than being handed the
    /// whole thing up front. This is the normal mode and the reason the project exists.
    pub const REQUIRE_RANGE: Self = Self(0);
    /// The host can move a window over a source that is larger than the guest can address, so a
    /// 32-bit guest is not limited to four gigabytes of input.
    pub const SLIDING_WINDOW: Self = Self(1);
    /// The decoder honours a column projection instead of producing every column and letting the
    /// host throw most of them away.
    pub const PROJECTION: Self = Self(2);
    /// The decoder honours a filter pushed down to it.
    pub const FILTER_PUSHDOWN: Self = Self(3);
    /// The decoder can start at an arbitrary row rather than only at the beginning.
    pub const RANDOM_ACCESS: Self = Self(4);
    /// The decoder keeps no state between calls, so the host is free to reuse one instance for
    /// unrelated scans or to run several scans against the same instance.
    pub const STATELESS: Self = Self(5);
    /// The decoder is prepared to be interrupted partway through and resumed, which is what lets a
    /// host put a time limit on a scan without killing it.
    pub const RESUMABLE: Self = Self(6);

    /// The name of this capability, if it is one we assigned.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        match self {
            Self::REQUIRE_RANGE => Some("require-range"),
            Self::SLIDING_WINDOW => Some("sliding-window"),
            Self::PROJECTION => Some("projection"),
            Self::FILTER_PUSHDOWN => Some("filter-pushdown"),
            Self::RANDOM_ACCESS => Some("random-access"),
            Self::STATELESS => Some("stateless"),
            Self::RESUMABLE => Some("resumable"),
            _ => None,
        }
    }
}

impl core::fmt::Display for Capability {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "capability bit {}", self.0),
        }
    }
}

/// A set of capabilities.
///
/// The set is a fixed 32 bytes, so it holds 256 capabilities and needs no allocation. On the wire
/// it is a variable-length byte string, so a future version can make it wider without breaking
/// anything, and [`CapabilitySet::has_bits_beyond_this_build`] is how this version notices that
/// happened instead of silently ignoring the extra bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CapabilitySet {
    bits: [u8; Self::BYTES],
    beyond: bool,
}

impl CapabilitySet {
    /// How many bytes of bitset this build carries.
    pub const BYTES: usize = 32;

    /// The highest capability bit this build can represent.
    pub const MAX_BIT: u16 = 255;

    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bits: [0; Self::BYTES],
            beyond: false,
        }
    }

    /// Adds a capability and returns the set, so sets can be built in one expression.
    ///
    /// A capability above [`CapabilitySet::MAX_BIT`] cannot be represented by this build and is
    /// ignored. That cannot happen by accident, because every capability this build knows about is
    /// a constant in this file.
    #[must_use]
    pub const fn with(mut self, cap: Capability) -> Self {
        if cap.0 <= Self::MAX_BIT {
            let byte = (cap.0 / 8) as usize;
            self.bits[byte] |= 1 << (cap.0 % 8);
        }
        self
    }

    /// Whether the set contains a capability.
    #[must_use]
    pub const fn contains(self, cap: Capability) -> bool {
        if cap.0 > Self::MAX_BIT {
            return false;
        }
        let byte = (cap.0 / 8) as usize;
        self.bits[byte] & (1 << (cap.0 % 8)) != 0
    }

    /// Whether the set is empty as far as this build can tell.
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.beyond && self.bits.iter().all(|b| *b == 0)
    }

    /// Whether the bytes this set was decoded from had bits set past what this build can hold.
    ///
    /// This matters on the required side. A decoder built against a later version of the ABI may
    /// require a capability that did not exist when this host was compiled, and a host that just
    /// truncated the bitset would conclude the decoder required nothing and run it anyway. That is
    /// the failure this flag exists to prevent.
    #[must_use]
    pub const fn has_bits_beyond_this_build(self) -> bool {
        self.beyond
    }

    /// Reads a set from its wire form.
    ///
    /// Bytes past what this build can hold are not stored, but if any of them are non-zero the set
    /// remembers that.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut out = Self::new();
        let take = bytes.len().min(Self::BYTES);
        out.bits[..take].copy_from_slice(&bytes[..take]);
        out.beyond = bytes[take..].iter().any(|b| *b != 0);
        out
    }

    /// The wire form, with trailing zero bytes trimmed off so an empty set costs nothing.
    ///
    /// Trimming is safe because a reader treats a missing byte as zero, which is what
    /// [`CapabilitySet::from_bytes`] does.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let end = self
            .bits
            .iter()
            .rposition(|b| *b != 0)
            .map_or(0, |i| i.saturating_add(1));
        &self.bits[..end]
    }

    /// The capabilities in this set that are not in `other`.
    ///
    /// The `beyond` flag rides along, because a bit this build cannot name is by definition a bit
    /// `other` does not offer.
    #[must_use]
    pub fn difference(self, other: Self) -> Self {
        let mut out = Self::new();
        for i in 0..Self::BYTES {
            out.bits[i] = self.bits[i] & !other.bits[i];
        }
        out.beyond = self.beyond;
        out
    }

    /// The capabilities in both sets.
    #[must_use]
    pub fn intersection(self, other: Self) -> Self {
        let mut out = Self::new();
        for i in 0..Self::BYTES {
            out.bits[i] = self.bits[i] & other.bits[i];
        }
        out
    }

    /// The capabilities in either set.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let mut out = Self::new();
        for i in 0..Self::BYTES {
            out.bits[i] = self.bits[i] | other.bits[i];
        }
        out.beyond = self.beyond || other.beyond;
        out
    }

    /// Every capability in the set, lowest bit first.
    ///
    /// Bits past [`CapabilitySet::MAX_BIT`] cannot be listed, which is what
    /// [`CapabilitySet::has_bits_beyond_this_build`] is for.
    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        (0..=Self::MAX_BIT)
            .map(Capability)
            .filter(move |c| self.contains(*c))
    }
}

// The two constants have to agree or the bounds checks in this file are wrong.
const _: () = assert!(CapabilitySet::BYTES * 8 == CapabilitySet::MAX_BIT as usize + 1);

impl Reader<'_> {
    /// Reads a length-prefixed capability set.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Reader::var_bytes`].
    pub fn capability_set(&mut self) -> crate::error::Result<CapabilitySet> {
        Ok(CapabilitySet::from_bytes(self.var_bytes()?))
    }
}
