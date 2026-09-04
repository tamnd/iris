//! Decoder identity, content hashes and substitution policy.
//!
//! A decoder is named by a URI and pinned by a BLAKE3 digest. A host that recognises the digest may
//! run its own native implementation instead, and a host that does not may fetch and verify.
//!
//! What is here now is the part that has to be right before any of the rest of it is worth having:
//! a container hands over its decoder module only after the module has been hashed and the hash has
//! matched. The prior art in this space stores a checksum and never checks it, which is the finding
//! this project exists to not repeat, so the check is not a policy a host opts into. It is the only
//! path to the bytes.
//!
//! What a host does opt into is where a decoder may come from. The default runs decoders embedded
//! in the container and nothing else, because a decoder named by a URI means a dataset can cause a
//! host to go and fetch something and then execute it. A host that means to allow that builds a
//! [`Policy`] with a [`Resolve`] of its own, which is to say it writes the thing that goes and
//! finds the module. Whatever comes back is hashed against the container's digest exactly like an
//! embedded module, so opting in changes where the bytes come from and changes nothing about
//! whether they are checked.
//!
//! ```
//! # use iris_abi::CapabilitySet;
//! # use iris_format::{Builder, Container, SectionKind};
//! let mut builder = Builder::new("readings", 3);
//! builder.section(SectionKind::Data, b"rows go here".to_vec());
//! builder.embed_decoder("test", (1, 0), CapabilitySet::new(), b"a module".to_vec());
//! let bytes = builder.build()?;
//!
//! let container = Container::parse(&bytes)?;
//! let decoder = iris_trust::decoder(&container)?;
//! assert_eq!(decoder.module(), b"a module");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Substitution, signatures and a policy about what may be fetched are still ahead. See the
//! milestone that owns this crate in `docs/ROADMAP.md`.

mod error;
mod policy;
mod verify;

pub use error::Untrusted;
pub use policy::{Policy, Resolve};
pub use verify::{Verified, decoder};

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
