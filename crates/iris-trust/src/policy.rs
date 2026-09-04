//! Where a host is willing to get a decoder from.

use std::fmt;
use std::sync::Arc;

use iris_format::DecoderRef;

/// Something that can produce the module for a decoder that is not in the container.
///
/// A resolver is whatever the host has: a directory of modules it ships, a cache, a registry
/// client, an operator who copied a file into place. This crate does not care which, and it does
/// not care whether the resolver checked anything, because whatever comes back is hashed against
/// the digest in the container before it goes anywhere.
///
/// Returning `None` is the ordinary answer for a decoder this host has no copy of. It is not an
/// error and it is not a refusal, it is a resolver saying it does not have that one.
pub trait Resolve: fmt::Debug + Send + Sync {
    /// Finds the module for a decoder, by whatever means this host has.
    ///
    /// The whole reference is passed rather than just the digest, because a resolver that goes and
    /// fetches something needs the name to fetch it by. The digest is what the answer is checked
    /// against, so a resolver that ignores the name and returns the wrong module is caught rather
    /// than trusted.
    fn resolve(&self, decoder: &DecoderRef<'_>) -> Option<Vec<u8>>;
}

/// What a host will run.
///
/// The default runs decoders embedded in the container and nothing else. That is the case the
/// format is designed around: the dataset carries the code that reads it, so there is nothing to
/// fetch and nothing to decide. A decoder named by a URI is a different proposition, because a
/// dataset that names one can cause a host to go and get something and then execute it, and this
/// crate will not do that unless a host has said so with a resolver of its own.
///
/// There is no boolean here on purpose. Turning external decoders on means writing the thing that
/// goes and finds them, which is not something anybody does by accident.
#[derive(Clone, Default)]
pub struct Policy {
    external: Option<Arc<dyn Resolve>>,
}

impl Policy {
    /// Embedded decoders and nothing else, which is the default.
    #[must_use]
    pub const fn embedded_only() -> Self {
        Self { external: None }
    }

    /// Also runs decoders that live outside the container, using this resolver to find them.
    ///
    /// The bytes the resolver returns are hashed and compared to the digest in the container in
    /// exactly the same way an embedded module is. A resolver that returns the wrong module, or a
    /// registry that has been tampered with, fails here rather than at the compiler.
    #[must_use]
    pub fn with_external_decoders_resolved_by(resolver: impl Resolve + 'static) -> Self {
        Self {
            external: Some(Arc::new(resolver)),
        }
    }

    /// The resolver this policy will use for a decoder that is not in the container, if any.
    #[must_use]
    pub fn resolver(&self) -> Option<&dyn Resolve> {
        self.external.as_deref()
    }
}

// Written out rather than derived because `Arc<dyn Resolve>` cannot be derived through, and because
// what a reader wants from a policy in a log line is whether external decoders are on, not the
// address of a trait object.
impl fmt::Debug for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.external {
            None => f.write_str("Policy { embedded decoders only }"),
            Some(resolver) => write!(f, "Policy {{ external decoders resolved by {resolver:?} }}"),
        }
    }
}
