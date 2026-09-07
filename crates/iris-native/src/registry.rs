//! The table a host looks a decoder up in.

use std::collections::HashMap;
use std::sync::Arc;

use iris_format::Digest;

use crate::native::Native;

/// Which decoders this host has its own implementation of.
///
/// One key and one kind of key: the digest of the decoder module. A host registers an
/// implementation against the exact bytes it is a rewrite of, and a container gets that
/// implementation only when the module it carries hashes to that value.
///
/// # Why there is no name in here
///
/// Because a name is a claim and a digest is a fact. A registry keyed on the name is a registry
/// where any dataset that calls its decoder `fixedwidth` gets whatever this host thinks
/// `fixedwidth` means, which is native code running against bytes nobody checked it against. The
/// dataset chooses its own name and there is nothing on the other side of that choice.
///
/// Keying on the digest closes it, and it closes it structurally rather than by being careful. The
/// digest the host looks up is not the one the container claims, it is the one `iris-trust`
/// computed from the module bytes that were actually there, so a container that names a known
/// digest while carrying different bytes has already been refused before this table is consulted.
/// There is no method here that takes a name, and adding one would mean adding a field.
///
/// # Sharing one
///
/// Cloning is a handle rather than a copy, and a registry is meant to be built once when the
/// process starts and handed to every runtime. Building it per query would work and would mean
/// hashing every module again for nothing.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    entries: Arc<HashMap<Digest, Arc<dyn Native>>>,
}

impl Registry {
    /// An empty registry, which is the one a host that has not opted in has.
    ///
    /// Every container opened against this one runs in the sandbox, which is the default and is the
    /// answer for a host that has no native code to offer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an implementation against the module it is a rewrite of.
    ///
    /// This is the way to do it. The digest is computed here from the bytes handed in, so what a
    /// host says is "this code decodes what that module decodes" and the identity is derived rather
    /// than typed. [`Registry::with_digest`] is for a host that has the digest and not the module,
    /// and it is the one where a wrong answer is possible.
    #[must_use]
    pub fn with_module(self, module: &[u8], native: Arc<dyn Native>) -> Self {
        self.with_digest(Digest::of(module), native)
    }

    /// Registers an implementation against a digest the host already has.
    ///
    /// For a host that keeps its modules somewhere else and does not want to read one in order to
    /// name it. The digest has to be the hash of the module bytes and nothing checks that here,
    /// which is why [`Registry::with_module`] exists and is the one to reach for.
    #[must_use]
    pub fn with_digest(mut self, digest: Digest, native: Arc<dyn Native>) -> Self {
        Arc::make_mut(&mut self.entries).insert(digest, native);
        self
    }

    /// The implementation registered for these module bytes, if there is one.
    ///
    /// The digest is the whole of the question. A caller that has a name and no digest has nothing
    /// to ask with, which is the point.
    #[must_use]
    pub fn get(&self, digest: &Digest) -> Option<Arc<dyn Native>> {
        self.entries.get(digest).map(Arc::clone)
    }

    /// How many implementations are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is registered, in which case every container runs in the sandbox.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use iris_abi::{Hello, ScanRequest};
    use iris_source::RangeSource;
    use iris_vm::Handshake;

    use super::Registry;
    use crate::native::{Native, Result, Scanning};

    /// Stands in for nothing and is never run. These tests are about the table.
    #[derive(Debug)]
    struct Stub;

    impl Native for Stub {
        fn handshake(&self, _hello: &Hello) -> Result<Handshake> {
            unreachable!("the registry tests never run a decoder")
        }

        fn scan<'a>(
            &'a self,
            _request: &'a ScanRequest<'a>,
            _source: &'a mut (dyn RangeSource + Send),
        ) -> Scanning<'a> {
            unreachable!("the registry tests never run a decoder")
        }
    }

    #[test]
    fn a_module_is_found_by_its_own_bytes_and_by_nothing_else() {
        let module = b"the bytes of a decoder, near enough for a lookup";
        let registry = Registry::new().with_module(module, Arc::new(Stub));

        assert_eq!(registry.len(), 1);
        assert!(registry.get(&iris_format::Digest::of(module)).is_some());

        // One byte different is a different decoder, and there is no sense in which it is nearly
        // the same one. That is the property the whole table rests on.
        let mut nearly = module.to_vec();
        nearly[0] ^= 1;
        assert!(registry.get(&iris_format::Digest::of(&nearly)).is_none());
    }

    #[test]
    fn an_empty_registry_knows_nothing() {
        let registry = Registry::new();
        assert!(registry.is_empty());
        assert!(
            registry
                .get(&iris_format::Digest::of(b"anything at all"))
                .is_none()
        );
    }

    #[test]
    fn registering_the_same_module_twice_keeps_the_second_one() {
        let module = b"one decoder";
        let registry = Registry::new()
            .with_module(module, Arc::new(Stub))
            .with_module(module, Arc::new(Stub));
        assert_eq!(registry.len(), 1);
    }
}
