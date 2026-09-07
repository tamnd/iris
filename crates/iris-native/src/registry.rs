//! The table a host looks a decoder up in.

use std::collections::HashMap;
use std::sync::Arc;

use iris_abi::CapabilitySet;
use iris_format::Digest;

use crate::differential::Kernel;
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
/// # Why the only thing it accepts is a proved kernel
///
/// Registering a native implementation asserts that it produces what running the module would have
/// produced, and that assertion is the reason substitution is allowed to happen at all. An
/// unverified one is worse than having no fast path, because the fast path is the one that runs.
/// So the only thing that goes in here is a [`Kernel`], and the only thing that makes a [`Kernel`]
/// is [`Differential::verify`](crate::Differential::verify). Code that registers an implementation
/// without a differential run does not compile.
///
/// # Sharing one
///
/// Cloning is a handle rather than a copy, and a registry is meant to be built once when the
/// process starts and handed to every runtime. Building it per query would work and would mean
/// running every differential again for nothing.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    entries: Arc<HashMap<Digest, Entry>>,
}

/// One implementation and the terms it was proved under.
#[derive(Clone, Debug)]
struct Entry {
    native: Arc<dyn Native>,
    offered: Vec<CapabilitySet>,
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

    /// Adds an implementation that has been proved against the module it stands in for.
    ///
    /// The digest is the one the differential run computed from the module bytes it compiled, so
    /// nothing here is typed by hand and there is no registration that names the wrong module and
    /// then silently never fires.
    #[must_use]
    pub fn with(mut self, kernel: Kernel) -> Self {
        let (digest, native, offered) = kernel.into_parts();
        Arc::make_mut(&mut self.entries).insert(digest, Entry { native, offered });
        self
    }

    /// The implementation registered for these module bytes under these terms, if there is one.
    ///
    /// The digest is most of the question and the terms are the rest of it. A kernel is proved
    /// under the capability sets it was run against, and a host offering a decoder something else
    /// is asking for behaviour nobody compared, so it gets the sandbox instead. That is the safe
    /// direction to fail in: a host that adds a capability to what it offers loses substitution
    /// until somebody runs the differential again, rather than keeping it on terms the kernel has
    /// never seen.
    ///
    /// A caller that has a name and no digest has nothing to ask with, which is the point.
    #[must_use]
    pub fn get(&self, digest: &Digest, offered: CapabilitySet) -> Option<Arc<dyn Native>> {
        self.entries
            .get(digest)
            .filter(|entry| entry.offered.contains(&offered))
            .map(|entry| Arc::clone(&entry.native))
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

    use iris_abi::{Capability, CapabilitySet, Hello, ScanRequest};
    use iris_format::Digest;
    use iris_source::RangeSource;
    use iris_vm::Handshake;

    use super::Registry;
    use crate::differential::Kernel;
    use crate::native::{Native, Result, Scanning};

    /// Stands in for nothing and is never run. These tests are about the table.
    ///
    /// They build their kernels with the constructor that skips the differential, which only exists
    /// while this crate's own tests are compiled. The runs that use a real module and a real kernel
    /// are in `iris-runtime`'s tests, because that is where a decoder gets built.
    #[derive(Debug)]
    struct Stub;

    impl Native for Stub {
        fn handshake(&self, _hello: &Hello) -> Result<Handshake> {
            unreachable!("the registry tests never run a decoder")
        }

        fn scan<'a>(
            &'a self,
            _hello: &'a Hello,
            _request: &'a ScanRequest<'a>,
            _source: &'a mut (dyn RangeSource + Send),
        ) -> Scanning<'a> {
            unreachable!("the registry tests never run a decoder")
        }
    }

    /// The terms the tests here register and look up under.
    fn terms() -> CapabilitySet {
        CapabilitySet::new().with(Capability::RANDOM_ACCESS)
    }

    /// A kernel for these module bytes, proved under [`terms`] as far as this table is concerned.
    fn kernel(module: &[u8]) -> Kernel {
        Kernel::untested(Digest::of(module), Arc::new(Stub), vec![terms()])
    }

    #[test]
    fn a_module_is_found_by_its_own_bytes_and_by_nothing_else() {
        let module = b"the bytes of a decoder, near enough for a lookup";
        let registry = Registry::new().with(kernel(module));

        assert_eq!(registry.len(), 1);
        assert!(registry.get(&Digest::of(module), terms()).is_some());

        // One byte different is a different decoder, and there is no sense in which it is nearly
        // the same one. That is the property the whole table rests on.
        let mut nearly = module.to_vec();
        nearly[0] ^= 1;
        assert!(registry.get(&Digest::of(&nearly), terms()).is_none());
    }

    #[test]
    fn an_empty_registry_knows_nothing() {
        let registry = Registry::new();
        assert!(registry.is_empty());
        assert!(
            registry
                .get(&Digest::of(b"anything at all"), terms())
                .is_none()
        );
    }

    #[test]
    fn registering_the_same_module_twice_keeps_the_second_one() {
        let module = b"one decoder";
        let registry = Registry::new().with(kernel(module)).with(kernel(module));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn terms_the_kernel_was_never_proved_under_get_the_sandbox() {
        let module = b"one decoder";
        let registry = Registry::new().with(kernel(module));

        // The same module, the same host, and one more capability on offer than anybody compared
        // this implementation under. The answer is nothing, which sends the container to the
        // sandbox rather than to code that has never been asked to project.
        let more = terms().with(Capability::PROJECTION);
        assert!(registry.get(&Digest::of(module), more).is_none());
        assert!(registry.get(&Digest::of(module), terms()).is_some());
    }
}
