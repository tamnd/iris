//! Getting a decoder module out of a container, which means hashing it.

use iris_format::{Container, DecoderLocation, DecoderRef, Digest};

use crate::error::Untrusted;

/// A decoder module that hashes to what the container says it should.
///
/// The point of this type is that it has no public constructor. [`decoder`] is the only thing that
/// makes one, and the only thing [`decoder`] does is hash the bytes and compare, so a caller
/// holding a `Verified` is holding proof that the comparison happened. That is a stronger promise
/// than a function that checks and then returns a slice, because the slice is the same slice
/// whether or not anybody called the checker.
///
/// It is deliberately not `Clone` from a module, only from another `Verified`. There is no way to
/// build one around bytes that were never hashed.
#[derive(Clone, Debug)]
pub struct Verified<'a> {
    record: DecoderRef<'a>,
    module: &'a [u8],
    digest: Digest,
}

impl<'a> Verified<'a> {
    /// The module, which is the same bytes that were hashed.
    #[must_use]
    pub const fn module(&self) -> &'a [u8] {
        self.module
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

/// Hashes the decoder in a container and hands it over only if the hash matches.
///
/// This is the whole of the trust boundary for an embedded decoder, and it is a function rather
/// than a method on a host so that there is nowhere to put a flag that turns it off. A host cannot
/// reach the module except through here, and here always hashes.
///
/// The hash is over the module alone. The container's root digest covers the header and the footer,
/// which is what makes a container cheap to open, so a byte changed inside the decoder section
/// parses perfectly well and is caught here instead. That is the case this function exists for.
///
/// # Errors
///
/// Returns [`Untrusted::Missing`] if the container names no decoder, [`Untrusted::External`] if the
/// module lives outside the container, [`Untrusted::Lost`] if the section it names is not in the
/// file, and [`Untrusted::Digest`], carrying both digests, if the bytes are not the module the
/// container names.
pub fn decoder<'a>(container: &Container<'a>) -> Result<Verified<'a>, Untrusted> {
    let record = container.decoder().ok_or(Untrusted::Missing)?;
    let DecoderLocation::Embedded { section } = record.location else {
        return Err(Untrusted::External);
    };
    let module = container
        .decoder_bytes()
        .ok_or(Untrusted::Lost { section })?;

    let found = Digest::of(module);
    let expected = record.digest;
    if found != expected {
        return Err(Untrusted::Digest { expected, found });
    }

    Ok(Verified {
        record: record.clone(),
        module,
        digest: found,
    })
}

#[cfg(test)]
mod tests {
    use iris_abi::CapabilitySet;
    use iris_format::{Builder, Container, DecoderLocation, Digest, SectionKind};

    use super::{Untrusted, decoder};

    /// Stands in for a decoder. Nothing here compiles it, and that is the point: the digest is
    /// checked before anything treats these bytes as code, so they do not have to be code.
    const MODULE: &[u8] = b"a module, as far as this crate is concerned";

    fn container() -> Vec<u8> {
        let mut builder = Builder::new("readings", 3);
        builder.section(SectionKind::Data, b"rows go here".to_vec());
        builder.embed_decoder("test", (1, 0), CapabilitySet::new(), MODULE.to_vec());
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
        let bytes = container();
        let container = Container::parse(&bytes).expect("the container parses");
        let verified = decoder(&container).expect("the module is the one the container names");

        assert_eq!(verified.module(), MODULE);
        assert_eq!(verified.digest(), Digest::of(MODULE));
        assert_eq!(verified.record().name, "test");
    }

    #[test]
    fn one_flipped_byte_in_the_module_is_refused_with_both_digests() {
        let mut bytes = container();
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
    fn a_decoder_that_is_not_in_the_file_is_not_this_crate_to_resolve() {
        let mut builder = Builder::new("readings", 3);
        builder.section(SectionKind::Data, b"rows go here".to_vec());
        builder.external_decoder(
            "elsewhere",
            (1, 0),
            CapabilitySet::new(),
            Digest::of(MODULE),
        );
        let bytes = builder.build().expect("a container this small always fits");
        let container = Container::parse(&bytes).expect("the container parses");

        assert!(matches!(
            container.decoder().map(|d| d.location),
            Some(DecoderLocation::External)
        ));
        assert_eq!(decoder(&container).unwrap_err(), Untrusted::External);
    }
}
