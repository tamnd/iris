//! Content digests.
//!
//! Every section carries one, the footer commits to all of them, and the trailer commits to the
//! footer. That chain is what makes it possible to say this is the same dataset without comparing
//! whole files, and it is what lets a host swap in a native decoder it already trusts for a
//! sandboxed one it does not: the substitution is keyed on the digest of the decoder module, so
//! there is nothing to guess about which decoder a dataset actually asked for.

use core::fmt;

use crate::layout::DIGEST_SIZE;

/// A BLAKE3 hash of some part of a container.
///
/// BLAKE3 rather than SHA-256 because this gets computed over whole datasets on the write path and
/// over whole sections on any read that verifies, and the difference is large enough to change
/// whether verification is on by default.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Digest(pub [u8; DIGEST_SIZE]);

impl Digest {
    /// The digest of some bytes.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// The raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; DIGEST_SIZE] {
        &self.0
    }

    /// The first sixteen hex characters, which is what belongs in a log line.
    ///
    /// Never use this to decide whether two things are the same. It is here because a full digest
    /// in an error message is noise a reader skips, and a short one is something they read.
    #[must_use]
    pub fn short(&self) -> String {
        let full = self.to_string();
        full[..16].to_owned()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.short())
    }
}
