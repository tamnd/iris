//! Opening a container and pulling batches out of it.

use std::sync::Arc;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use iris_abi::{
    ABI_MAJOR, ABI_MINOR, Agreement, Capability, CapabilitySet, Hello, HelloAck, ScanRequest,
    Writer, negotiate,
};
use iris_format::{Container, SchemaEncoding, SectionKind};
use iris_trust::Policy;
use iris_vm::{Decoder, Program, Vm};

use crate::assemble::record_batch;
use crate::error::{Error, Result};
use crate::schema::{describe, schema_from_ipc};

/// The largest record this host will build for a decoder.
///
/// A `Hello` and a `ScanRequest` are both small and neither grows with the data, so this is a bound
/// on a mistake rather than a bound on a workload.
const RECORD_LIMIT: usize = 1 << 20;

/// What this host can do.
///
/// The source is resident and copied into the guest whole, so random access is free and the decoder
/// can be told so. Projection is offered because a decoder that can skip columns should, and a
/// decoder that cannot will ignore it.
const OFFERED: CapabilitySet = CapabilitySet::new()
    .with(Capability::RANDOM_ACCESS)
    .with(Capability::PROJECTION);

/// A compiler, and the terms this host offers a decoder.
///
/// One of these is meant to be shared and reused. It holds the Wasmtime engine, which is expensive
/// to build and caches across every module it compiles.
#[derive(Clone, Debug)]
pub struct Runtime {
    vm: Vm,
    max_batch_rows: u64,
    policy: Policy,
}

impl Runtime {
    /// A runtime with the default terms.
    ///
    /// The default runs decoders embedded in the container and nothing else. See
    /// [`Runtime::with_decoder_policy`] for what changing that involves.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vm`] if the engine cannot be built with the settings a decoder runs under.
    pub fn new() -> Result<Self> {
        Ok(Self {
            vm: Vm::new()?,
            max_batch_rows: 8192,
            policy: Policy::embedded_only(),
        })
    }

    /// Says where this host will accept a decoder from.
    ///
    /// The default is embedded decoders only, which is the case the format is built around: the
    /// dataset carries the code that reads it, so nothing is fetched and there is nothing to
    /// decide. A dataset that names a decoder by URI is asking this host to go and get something
    /// and then run it, and that is a decision an operator makes rather than one a file makes.
    ///
    /// Allowing it means handing [`iris_trust::Policy`] a resolver, which is to say writing the
    /// thing that finds the module. Whatever it returns is hashed against the digest in the
    /// container in exactly the same way an embedded module is, so this changes where the bytes
    /// come from and changes nothing about whether they are checked.
    #[must_use]
    pub fn with_decoder_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Sets how long one call into a decoder may take before it is stopped.
    ///
    /// Every call is metered and there is no way to ask for one that is not, so this moves the
    /// budget rather than deciding whether there is one. The default is ten seconds, which is a
    /// bound on a decoder that never returns rather than a bound on a decoder doing work: reading
    /// eight thousand rows out of a resident buffer is milliseconds.
    ///
    /// A host serving interactive queries has a much better number for this than a default does, and
    /// the cost of getting it wrong is an [`Error::Vm`] carrying [`iris_vm::Error::Deadline`], which
    /// names both the decoder and the budget it was given.
    #[must_use]
    pub fn with_decoder_deadline(mut self, deadline: Duration) -> Self {
        self.vm = self.vm.with_deadline(deadline);
        self
    }

    /// Sets the largest batch this host will ask for.
    ///
    /// A decoder is told this number and is expected to respect it, because the host is the side
    /// that knows how much memory it is willing to have in flight. Zero is treated as one, since a
    /// decoder that batches zero rows at a time never finishes.
    #[must_use]
    pub const fn with_max_batch_rows(mut self, rows: u64) -> Self {
        self.max_batch_rows = if rows == 0 { 1 } else { rows };
        self
    }

    /// Opens a container, compiles the decoder in it, and reads its schema.
    ///
    /// The decoder module is hashed and checked against the container before it is compiled, which
    /// is the order that matters: compiling is the first thing that treats those bytes as code.
    ///
    /// Nothing else is verified here. Hashing every section reads the whole file, which is a
    /// decision for whoever accepted the dataset rather than something that should happen on every
    /// open, and [`iris_format::Container::verify`] is where that decision gets made.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Container`] if the bytes are not a container, [`Error::Trust`] if there is
    /// no decoder here to run or the module is not the one the container names,
    /// [`Error::SchemaEncoding`] if the schema is missing or in an encoding this build does not
    /// read, and [`Error::Abi`] if the decoder was built against a major ABI version this host does
    /// not speak.
    pub fn open<'a>(&self, bytes: &'a [u8]) -> Result<Dataset<'a>> {
        let container = Container::parse(bytes)?;

        // The module arrives already hashed, because iris-trust is the only way to get one and
        // hashing is all it does. Nothing downstream of this line can ask for the unverified bytes,
        // which is the whole design: there is no flag here to turn off, and adding one would mean
        // adding a function to another crate first.
        let verified = self.policy.decoder(&container)?;
        let decoder = verified.record();
        let module = verified.module();
        let schema = match container.schema() {
            Some(schema) if schema.encoding == SchemaEncoding::ArrowIpc => {
                Arc::new(schema_from_ipc(schema.bytes)?)
            }
            Some(schema) => return Err(Error::SchemaEncoding(format!("{:?}", schema.encoding))),
            None => return Err(Error::SchemaEncoding("missing".to_owned())),
        };

        // The schema is checked before the ABI, which is the less obvious of the two orderings
        // here. A schema this host cannot walk would be refused whatever ABI the decoder wanted, so
        // nothing is lost by refusing it first, and the ABI message promises to describe the
        // schema. Describing a schema before checking it means formatting a type that may be
        // nested past anything a formatter will survive, which turns a refusal into a crash.
        iris_guard::check_schema(&schema)?;

        // The ABI is checked here rather than left to the handshake, for two reasons. A decoder
        // built against a major version this host does not speak is never going to agree on terms,
        // so compiling it first is work thrown away on the way to the same answer. And the message
        // an operator gets to keep is much better from here: at this point the container has already
        // given up the decoder's name, its digest and the schema, and none of those are in scope by
        // the time a refusal comes back out of the guest.
        if decoder.abi_major != ABI_MAJOR {
            return Err(Error::Abi {
                needed_major: decoder.abi_major,
                needed_minor: decoder.abi_minor,
                host_major: ABI_MAJOR,
                host_minor: ABI_MINOR,
                name: decoder.name.to_owned(),
                digest: verified.digest().to_string(),
                schema: describe(&schema),
            });
        }

        // The digest goes with the module, so that a decoder that traps or runs away is named in the
        // error by the one identity it did not choose for itself. The name in the container is what
        // the decoder calls itself, and a decoder that has been swapped would still be called that.
        let program = self.vm.compile(module, &verified.digest().to_string())?;

        // M1 hands the decoder one run of bytes and calls it the source, so a container with two
        // data sections has no unambiguous answer to what the decoder should see. Refusing is
        // better than picking one.
        let data: Vec<_> = container
            .sections()
            .iter()
            .filter(|s| s.kind == SectionKind::Data)
            .collect();
        let [section] = data.as_slice() else {
            return Err(Error::DataSections(data.len()));
        };
        let source = container.section_bytes(section);

        let rows = container.dataset().rows;
        let name = container.dataset().name.clone();

        Ok(Dataset {
            program,
            schema,
            source,
            rows,
            name,
            max_batch_rows: self.max_batch_rows,
        })
    }
}

/// An open dataset, with its decoder compiled and its schema read.
///
/// Compiling happens once and instantiating happens per scan, which is the split Wasmtime is built
/// around: a fresh instance per scan means one scan cannot see what another one left behind, and it
/// costs a fraction of what compiling costs.
#[derive(Clone, Debug)]
pub struct Dataset<'a> {
    program: Program,
    schema: SchemaRef,
    source: &'a [u8],
    rows: u64,
    name: String,
    max_batch_rows: u64,
}

impl Dataset<'_> {
    /// The Arrow schema the container carries.
    #[must_use]
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// How many rows the container says it has.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// The dataset's name, which nothing here interprets.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads every row.
    ///
    /// # Errors
    ///
    /// See [`Dataset::scan_rows`].
    pub fn scan(&self) -> Result<Vec<RecordBatch>> {
        self.scan_rows(0, self.rows)
    }

    /// Reads a range of rows.
    ///
    /// A range that starts past the end produces no batches and is not an error, which is the same
    /// answer any other empty result gives.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] if the decoder and this host cannot agree on terms,
    /// [`Error::Vm`] if the decoder trapped or declined the request, and [`Error::Shape`] if a
    /// batch it produced does not match the schema.
    pub fn scan_rows(&self, start: u64, count: u64) -> Result<Vec<RecordBatch>> {
        let mut decoder = Decoder::instantiate(&self.program)?;
        decoder.load_source(self.source)?;

        let hello = self.hello();
        let handshake = decoder.start(&record(|w| hello.encode(w))?)?;

        // The decoder has already said yes by this point, and this is the host saying yes back.
        // Both sides check, because a decoder that agrees to terms it cannot meet and a host that
        // runs a decoder it cannot serve are different bugs and only one of them is ours.
        let ack = HelloAck {
            abi_major: handshake.abi_major,
            abi_minor: handshake.abi_minor,
            required: handshake.required,
            optional: handshake.optional,
            decoder_id: &handshake.decoder_id,
        };
        let _agreement: Agreement =
            negotiate(&hello, &ack).map_err(|refusal| Error::refused(&refusal))?;

        let request = ScanRequest {
            row_start: start,
            row_count: count,
            ..ScanRequest::everything()
        };
        let raw = decoder.scan(&record(|w| request.encode(w))?)?;

        let mut batches = Vec::with_capacity(raw.len());
        for batch in &raw {
            // An empty batch is how a decoder says there are no more rows. It has no arrays, so
            // there is nothing to assemble and nothing to check against the schema.
            if batch.rows == 0 && batch.nodes.is_empty() {
                continue;
            }
            batches.push(record_batch(&self.schema, batch)?);
        }
        Ok(batches)
    }

    fn hello(&self) -> Hello {
        Hello {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            // Zero means the whole source is visible, which it is: it was copied into the guest in
            // one piece. That stops being true at M4 and no decoder changes when it does.
            window_bytes: 0,
            max_batch_rows: self.max_batch_rows,
            offered: OFFERED,
            source_bytes: self.source.len() as u64,
        }
    }
}

/// Writes a record into a fresh buffer, growing until it fits.
///
/// The `iris-abi` writer never grows its own buffer, which is what makes it usable from a guest
/// with no allocator. On this side the cost of guessing wrong is one memcpy of a record that is
/// measured in tens of bytes, so guessing and retrying is simpler than computing the size twice and
/// keeping the two computations in agreement.
fn record(body: impl Fn(&mut Writer<'_>) -> iris_abi::Result<()>) -> Result<Vec<u8>> {
    let mut out = vec![0u8; 256];
    loop {
        let mut writer = Writer::new(&mut out);
        match body(&mut writer) {
            Ok(()) => {
                let written = writer.position();
                out.truncate(written);
                return Ok(out);
            }
            Err(iris_abi::Error::BufferFull { .. }) if out.len() < RECORD_LIMIT => {
                let room = out.len() * 2;
                out.resize(room, 0);
            }
            Err(err) => return Err(err.into()),
        }
    }
}
