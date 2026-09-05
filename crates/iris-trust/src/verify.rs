//! Getting a decoder module out of a container, which means hashing it.

use std::borrow::Cow;

use iris_format::{Container, DecoderLocation, DecoderRef, Digest};

use crate::error::Untrusted;
use crate::policy::Policy;

/// A decoder module that hashes to what the container says it should.
///
/// The point of this type is that it has no public constructor. [`Policy::decoder`] is the only
/// thing that makes one, and hashing the bytes is the only thing it does, so a caller holding a
/// `Verified` is holding proof that the comparison happened. That is a stronger promise than a
/// function that checks and then returns a slice, because the slice is the same slice whether or
/// not anybody called the checker.
///
/// The module is borrowed from the container when it is embedded and owned when a resolver went and
/// found it, which is the only difference the two cases make once the hash has matched.
#[derive(Clone, Debug)]
pub struct Verified<'a> {
    record: DecoderRef<'a>,
    module: Cow<'a, [u8]>,
    digest: Digest,
}

impl<'a> Verified<'a> {
    /// The module, which is the same bytes that were hashed.
    #[must_use]
    pub fn module(&self) -> &[u8] {
        &self.module
    }

    /// The digest of the module.
    ///
    /// This is the decoder's identity: the container names it, this crate recomputed it, and the
    /// two agreed. A host with a native implementation of this exact module looks it up by this
    /// value and runs that instead, which is what makes substitution safe rather than a matter of
    /// trusting a name.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// What the container says about the decoder: its name, its ABI version and what it needs.
    #[must_use]
    pub const fn record(&self) -> &DecoderRef<'a> {
        &self.record
    }
}

impl Policy {
    /// Finds the decoder this container names and hands it over only if it hashes to what the
    /// container says.
    ///
    /// This is the whole of the trust boundary for a decoder, and hashing is not a step it can be
    /// asked to skip. There is no flag here, and there is nowhere else to get the bytes.
    ///
    /// The hash is over the module alone. The container's root digest covers the header and the
    /// footer, which is what makes a container cheap to open, so a byte changed inside the decoder
    /// section parses perfectly well and is caught here instead. That is the case this exists for.
    ///
    /// A decoder that lives outside the container is refused unless this policy was built with a
    /// resolver. Whatever the resolver returns is hashed exactly like an embedded module, so a
    /// registry that hands back the wrong thing fails here rather than at the compiler.
    ///
    /// # Errors
    ///
    /// Returns [`Untrusted::Missing`] if the container names no decoder, [`Untrusted::External`] if
    /// the module lives outside the container and this policy has no resolver,
    /// [`Untrusted::Unresolved`] if it has one and the resolver found nothing, [`Untrusted::Lost`]
    /// if an embedded module names a section that is not in the file, and [`Untrusted::Digest`],
    /// carrying both digests, if the bytes are not the module the container names.
    pub fn decoder<'a>(&self, container: &Container<'a>) -> Result<Verified<'a>, Untrusted> {
        let record = container.decoder().ok_or(Untrusted::Missing)?;
        self.check(record, container.decoder_bytes().map(Cow::Borrowed))
    }

    /// The same check, for a host that read the module out of the file itself.
    ///
    /// A host that is not holding the container cannot be handed a slice of it, so it reads the
    /// section named by [`iris_format::Directory::decoder_section`] and passes the bytes here.
    /// `embedded` is `None` when there is no such section, which is the same thing as an embedded
    /// record naming a section the file does not have.
    ///
    /// Everything after that point is identical, deliberately. The bytes are hashed and compared
    /// against the record the same way whether they arrived as a borrow of a resident file, as a
    /// read through a window, or from a resolver, because how they were obtained is exactly the
    /// thing the digest exists to stop mattering.
    ///
    /// # Errors
    ///
    /// See [`Policy::decoder`].
    pub fn decoder_read<'a>(
        &self,
        record: &DecoderRef<'a>,
        embedded: Option<Vec<u8>>,
    ) -> Result<Verified<'a>, Untrusted> {
        self.check(record, embedded.map(Cow::Owned))
    }

    /// The comparison both entry points end in, written once so they cannot come to differ.
    fn check<'a>(
        &self,
        record: &DecoderRef<'a>,
        embedded: Option<Cow<'a, [u8]>>,
    ) -> Result<Verified<'a>, Untrusted> {
        let expected = record.digest;

        let module: Cow<'a, [u8]> = match record.location {
            DecoderLocation::Embedded { section } => embedded.ok_or(Untrusted::Lost { section })?,
            DecoderLocation::External => {
                let resolver = self.resolver().ok_or_else(|| Untrusted::External {
                    name: record.name.to_owned(),
                })?;
                let found = resolver
                    .resolve(record)
                    .ok_or_else(|| Untrusted::Unresolved {
                        name: record.name.to_owned(),
                        digest: expected,
                    })?;
                Cow::Owned(found)
            }
            // `DecoderLocation` is open ended, so this is a file written by something newer than
            // this build. Failing closed is the only reading of it that cannot be wrong.
            _ => {
                return Err(Untrusted::Elsewhere {
                    name: record.name.to_owned(),
                });
            }
        };

        let found = Digest::of(&module);
        if found != expected {
            return Err(Untrusted::Digest { expected, found });
        }

        Ok(Verified {
            record: record.clone(),
            module,
            digest: found,
        })
    }
}

/// Hashes the decoder embedded in a container and hands it over only if the hash matches.
///
/// This is [`Policy::decoder`] under the default policy, which runs embedded decoders and nothing
/// else. A host that means to run a decoder from somewhere else says so by building a [`Policy`]
/// with a resolver.
///
/// # Errors
///
/// See [`Policy::decoder`].
pub fn decoder<'a>(container: &Container<'a>) -> Result<Verified<'a>, Untrusted> {
    Policy::embedded_only().decoder(container)
}

#[cfg(test)]
mod tests {
    use iris_abi::CapabilitySet;
    use iris_format::{Builder, Container, DecoderRef, Digest, SectionKind};

    use super::{Policy, Untrusted, decoder};

    /// Stands in for a decoder. Nothing here compiles it, and that is the point: the digest is
    /// checked before anything treats these bytes as code, so they do not have to be code.
    const MODULE: &[u8] = b"a module, as far as this crate is concerned";

    /// A resolver that hands back whatever it was built with, which is how a host that keeps its
    /// decoders in a directory behaves once the file has been read.
    #[derive(Debug)]
    struct Holding(Option<Vec<u8>>);

    impl super::super::Resolve for Holding {
        fn resolve(&self, _decoder: &DecoderRef<'_>) -> Option<Vec<u8>> {
            self.0.clone()
        }
    }

    fn embedded() -> Vec<u8> {
        let mut builder = Builder::new("readings", 3);
        builder.section(SectionKind::Data, b"rows go here".to_vec());
        builder.embed_decoder("test", (1, 0), CapabilitySet::new(), MODULE.to_vec());
        builder.build().expect("a container this small always fits")
    }

    fn external() -> Vec<u8> {
        let mut builder = Builder::new("readings", 3);
        builder.section(SectionKind::Data, b"rows go here".to_vec());
        builder.external_decoder(
            "elsewhere",
            (1, 0),
            CapabilitySet::new(),
            Digest::of(MODULE),
        );
        builder.build().expect("a container this small always fits")
    }

    /// Where the module sits in the file, which is where a tamperer would be working.
    fn module_at(bytes: &[u8]) -> usize {
        bytes
            .windows(MODULE.len())
            .position(|window| window == MODULE)
            .expect("the builder wrote the module into the file")
    }

    #[test]
    fn a_module_that_matches_its_digest_is_handed_over() {
        let bytes = embedded();
        let container = Container::parse(&bytes).expect("the container parses");
        let verified = decoder(&container).expect("the module is the one the container names");

        assert_eq!(verified.module(), MODULE);
        assert_eq!(verified.digest(), Digest::of(MODULE));
        assert_eq!(verified.record().name, "test");
    }

    #[test]
    fn one_flipped_byte_in_the_module_is_refused_with_both_digests() {
        let mut bytes = embedded();
        let at = module_at(&bytes) + MODULE.len() / 2;
        bytes[at] ^= 1;

        // The file still parses, which is the part worth saying out loud. The root digest covers
        // the header and the footer, so a byte changed inside a section is not something the
        // container can notice, and the decoder digest is what stands between that byte and the
        // compiler.
        let container = Container::parse(&bytes).expect("the container still parses");

        let Err(Untrusted::Digest { expected, found }) = decoder(&container) else {
            panic!("a module with a flipped byte was accepted");
        };
        assert_eq!(expected, Digest::of(MODULE));
        assert_ne!(found, expected);

        let message = Untrusted::Digest { expected, found }.to_string();
        assert!(
            message.contains(&expected.to_string()),
            "the message does not say which module was expected: {message}"
        );
        assert!(
            message.contains(&found.to_string()),
            "the message does not say what arrived instead: {message}"
        );
    }

    #[test]
    fn a_container_with_no_decoder_says_so() {
        let mut builder = Builder::new("readings", 3);
        builder.section(SectionKind::Data, b"rows go here".to_vec());
        let bytes = builder.build().expect("a container this small always fits");
        let container = Container::parse(&bytes).expect("the container parses");

        assert_eq!(decoder(&container).unwrap_err(), Untrusted::Missing);
    }

    #[test]
    fn the_default_policy_refuses_a_decoder_that_is_not_in_the_container() {
        let bytes = external();
        let container = Container::parse(&bytes).expect("the container parses");

        let error = decoder(&container).unwrap_err();
        assert_eq!(
            error,
            Untrusted::External {
                name: "elsewhere".to_owned()
            }
        );

        // The refusal has to say what would have allowed it, because the alternative is an operator
        // reading the source of this crate to find out.
        let message = error.to_string();
        assert!(
            message.contains("Policy::with_external_decoders_resolved_by"),
            "the message does not name the setting that would allow this: {message}"
        );
    }

    #[test]
    fn a_host_that_opted_in_gets_the_module_its_resolver_found() {
        let bytes = external();
        let container = Container::parse(&bytes).expect("the container parses");
        let policy = Policy::with_external_decoders_resolved_by(Holding(Some(MODULE.to_vec())));

        let verified = policy
            .decoder(&container)
            .expect("the resolver returned the module the container names");
        assert_eq!(verified.module(), MODULE);
        assert_eq!(verified.digest(), Digest::of(MODULE));
    }

    #[test]
    fn a_resolver_that_returns_the_wrong_module_is_caught_by_the_digest() {
        let bytes = external();
        let container = Container::parse(&bytes).expect("the container parses");
        let policy = Policy::with_external_decoders_resolved_by(Holding(Some(
            b"some other module entirely".to_vec(),
        )));

        let Err(Untrusted::Digest { expected, found }) = policy.decoder(&container) else {
            panic!("a fetched module nobody checked was accepted");
        };
        assert_eq!(expected, Digest::of(MODULE));
        assert_ne!(found, expected);
    }

    #[test]
    fn a_module_read_out_of_a_file_is_checked_the_same_way() {
        let bytes = embedded();
        let container = Container::parse(&bytes).expect("the container parses");
        let record = container.decoder().expect("the container names a decoder");

        // What a windowed host does: it read the decoder section itself and owns the bytes, so it
        // cannot hand over a borrow of a file it is not holding.
        let read = container
            .decoder_bytes()
            .expect("the section is here")
            .to_vec();
        let verified = Policy::embedded_only()
            .decoder_read(record, Some(read))
            .expect("the module is the one the container names");

        assert_eq!(verified.module(), MODULE);
        assert_eq!(verified.digest(), Digest::of(MODULE));
        assert_eq!(verified.record().name, "test");

        // And the same answer as the resident path, which is the claim worth pinning down. Two
        // entry points that agree on a good file and differ on a bad one would be worse than one.
        assert_eq!(
            decoder(&container)
                .expect("the resident path agrees")
                .digest(),
            verified.digest()
        );
    }

    #[test]
    fn a_module_read_wrong_is_refused_by_the_digest_and_not_by_where_it_came_from() {
        let bytes = embedded();
        let container = Container::parse(&bytes).expect("the container parses");
        let record = container.decoder().expect("the container names a decoder");

        let mut read = container
            .decoder_bytes()
            .expect("the section is here")
            .to_vec();
        read[MODULE.len() / 2] ^= 1;

        let Err(Untrusted::Digest { expected, found }) =
            Policy::embedded_only().decoder_read(record, Some(read))
        else {
            panic!("a module that was read wrong was accepted");
        };
        assert_eq!(expected, Digest::of(MODULE));
        assert_ne!(found, expected);
    }

    #[test]
    fn an_embedded_record_with_nothing_read_says_the_section_is_lost() {
        let bytes = embedded();
        let container = Container::parse(&bytes).expect("the container parses");
        let record = container.decoder().expect("the container names a decoder");

        // A host reaches this by finding no section with the id the record names, which is a file
        // that points at a decoder it does not contain.
        assert_eq!(
            Policy::embedded_only()
                .decoder_read(record, None)
                .unwrap_err(),
            Untrusted::Lost { section: 1 }
        );
    }

    #[test]
    fn a_resolver_that_finds_nothing_is_not_an_attack() {
        let bytes = external();
        let container = Container::parse(&bytes).expect("the container parses");
        let policy = Policy::with_external_decoders_resolved_by(Holding(None));

        assert_eq!(
            policy.decoder(&container).unwrap_err(),
            Untrusted::Unresolved {
                name: "elsewhere".to_owned(),
                digest: Digest::of(MODULE),
            }
        );
    }
}
