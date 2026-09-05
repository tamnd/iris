//! Opening a container and pulling batches out of it.

use std::sync::Arc;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use iris_abi::{
    ABI_MAJOR, ABI_MINOR, Agreement, Capability, CapabilitySet, Hello, HelloAck, ScanRequest,
    Writer, negotiate,
};
use iris_format::layout::HEADER_SIZE;
use iris_format::{Container, Digest, Directory, Placement, SchemaEncoding, Section, SectionKind};
use iris_source::{RangeSource, Segment, Traffic, read_blocking};
use iris_trust::{Policy, Verified};
use iris_vm::{Decoder, Program, Vm};

use crate::assemble::record_batch;
use crate::error::{Error, Result};
use crate::schema::{describe, schema_from_ipc};

/// The largest record this host will build for a decoder.
///
/// A `Hello` and a `ScanRequest` are both small and neither grows with the data, so this is a bound
/// on a mistake rather than a bound on a workload.
const RECORD_LIMIT: usize = 1 << 20;

/// What this host can do when it holds the whole container.
///
/// The source is resident and copied into the guest whole, so random access is free and the decoder
/// can be told so. Projection is offered because a decoder that can skip columns should, and a
/// decoder that cannot will ignore it.
///
/// `require-range` is deliberately not here. The import exists on both paths, because it is part of
/// the ABI rather than part of a host, but this path attaches nothing to serve it, so a decoder that
/// needs to pull its own bytes has to be told that it is in the wrong place. Finding that out during
/// the handshake is much better than finding it out from the first range that fails.
const OFFERED: CapabilitySet = CapabilitySet::new()
    .with(Capability::RANDOM_ACCESS)
    .with(Capability::PROJECTION);

/// What this host can do when the container stays where it is.
///
/// Everything the resident path offers, plus the two bits that describe pulling. `require-range`
/// says the decoder may ask for bytes it was not given, and `sliding-window` says it will not be
/// given all of them at once, which is the same thing the non zero `window_bytes` in the handshake
/// says and is here so that a decoder can refuse on a bit rather than on a number.
const OFFERED_WINDOWED: CapabilitySet = OFFERED
    .with(Capability::REQUIRE_RANGE)
    .with(Capability::SLIDING_WINDOW);

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
        let opened = self.prepare(container.directory(), &verified)?;
        let source = container.section_bytes(data_section(container.directory())?);

        Ok(Dataset {
            program: opened.program,
            schema: opened.schema,
            source,
            rows: opened.rows,
            name: opened.name,
            digest: opened.digest,
            max_batch_rows: self.max_batch_rows,
        })
    }

    /// Opens a container that stays where it is, read one range at a time.
    ///
    /// This is the path for a dataset that does not fit anywhere it could be held: bigger than
    /// memory, bigger than a 32-bit guest can address, or simply not worth copying when a query
    /// touches a hundredth of it. Nothing is read here except the header, the footer and the decoder
    /// module, which together are about a kilobyte whatever the file is, and the payload is read by
    /// the decoder asking for it while it decodes.
    ///
    /// The decoder is not told any of this and does not have a way to find out. It is shown the data
    /// section addressed from zero, exactly as the resident path shows it a slice, and the two
    /// differences it can observe are that the handshake names a window size and that a range may
    /// take a while to arrive.
    ///
    /// # Errors
    ///
    /// Everything [`Runtime::open`] returns, plus [`Error::Source`] if one of the three ranges this
    /// needs in order to open the dataset could not be read.
    pub fn open_windowed(&self, source: Box<dyn RangeSource + Send>) -> Result<Windowed> {
        let mut source = source;
        let file_len = source.len();

        // The trailer first, because it is the only part of a container that can be found without
        // being told where it is, and it says where the footer is. Then the header and the footer,
        // which is everything the metadata is made of.
        let trailer_at = Placement::trailer_at(file_len)?;
        let trailer = read(source.as_mut(), trailer_at, Placement::TRAILER_LEN)?;
        let placement = Placement::read(&trailer, file_len)?;
        let header = read(source.as_mut(), 0, HEADER_SIZE)?;
        let footer = read(
            source.as_mut(),
            placement.footer_at(),
            placement.footer_len(),
        )?;
        let directory = Directory::parse(&header, &footer, placement)?;

        // The decoder module is read whole, because compiling it means having all of it, and it is
        // the one section that is small by construction. Reading it here rather than handing the
        // trust crate a source is what keeps that crate free of any opinion about where bytes live.
        let embedded = match directory.decoder_section() {
            Some(section) => Some(read(
                source.as_mut(),
                section.offset,
                section_len(section)?,
            )?),
            None => None,
        };
        let record = directory.decoder().ok_or(iris_trust::Untrusted::Missing)?;
        let verified = self.policy.decoder_read(record, embedded)?;
        let opened = self.prepare(&directory, &verified)?;

        let section = data_section(&directory)?;
        let (at, len) = (section.offset, section.len);
        // Everything below borrows nothing from the footer, so the metadata can go now and the
        // handle that is left is a program, a schema and a source.
        let window_bytes = source.largest().unwrap_or(0) as u64;
        let data = Segment::new(source, at, len)?;

        Ok(Windowed {
            program: opened.program,
            schema: opened.schema,
            source: Some(Box::new(data)),
            window_bytes,
            source_bytes: len,
            rows: opened.rows,
            name: opened.name,
            digest: opened.digest,
            max_batch_rows: self.max_batch_rows,
            last_scan: Traffic::NONE,
        })
    }

    /// Everything the two paths do between having the metadata and having a compiled decoder.
    ///
    /// It is one function because the checks and the order they happen in are the interesting part,
    /// and two copies of an ordering is two orderings waiting to drift.
    fn prepare(&self, directory: &Directory<'_>, verified: &Verified<'_>) -> Result<Opened> {
        let decoder = verified.record();
        let schema = match directory.schema() {
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
        let program = self
            .vm
            .compile(verified.module(), &verified.digest().to_string())?;

        Ok(Opened {
            program,
            schema,
            rows: directory.dataset().rows,
            name: directory.dataset().name.clone(),
            digest: verified.digest(),
        })
    }
}

/// What both open paths have once the metadata has been read and the decoder compiled.
struct Opened {
    program: Program,
    schema: SchemaRef,
    rows: u64,
    name: String,
    digest: Digest,
}

/// The one section a decoder is shown.
///
/// This host hands the decoder one run of bytes and calls it the source, so a container with two
/// data sections has no unambiguous answer to what the decoder should see. Refusing is better than
/// picking one.
fn data_section<'d>(directory: &'d Directory<'_>) -> Result<&'d Section> {
    let data: Vec<_> = directory
        .sections()
        .iter()
        .filter(|s| s.kind == SectionKind::Data)
        .collect();
    let [section] = data.as_slice() else {
        return Err(Error::DataSections(data.len()));
    };
    Ok(section)
}

/// How long a section is, as a length this machine can ask for in one read.
fn section_len(section: &Section) -> Result<usize> {
    usize::try_from(section.len).map_err(|_| {
        Error::Container(iris_format::Error::TooLarge {
            what: "a section this host has to read whole",
            needed: section.len,
        })
    })
}

/// Reads a range and waits for it.
///
/// Opening is the one place this host is allowed to block, because there is nothing else it could
/// be doing: no decoder has been compiled and no rows have been asked for. Once a scan is running
/// the same wait belongs to the caller, which is what [`iris_vm::Running`] is for.
fn read(source: &mut dyn RangeSource, at: u64, len: usize) -> Result<Vec<u8>> {
    Ok(read_blocking(source, at, len)?.to_vec())
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
    digest: Digest,
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

    /// The identity of the decoder that ran, which is the hash of its bytes.
    ///
    /// This is what the container promised and what iris-trust checked before anything compiled it,
    /// so it names the code rather than naming what the code calls itself. Two datasets that report
    /// the same digest ran the same decoder, whatever they were opened from and wherever their bytes
    /// happened to be, which is the only way a caller can say that from the outside.
    #[must_use]
    pub const fn decoder_digest(&self) -> Digest {
        self.digest
    }

    /// What the last scan cost, which on this path is nothing.
    ///
    /// Always zero, and that is the point rather than an omission. The bytes were already resident
    /// before this dataset existed: whoever produced the buffer paid for it, in full, whether or
    /// not the scan went on to read a hundredth of it. A windowed dataset reports a real pair of
    /// numbers here, and the comparison between the two is the whole argument for declaring ranges.
    ///
    /// See [`Windowed::last_scan`].
    #[must_use]
    pub const fn last_scan(&self) -> Traffic {
        Traffic::NONE
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
        run(&mut decoder, &self.hello(), &self.schema, start, count)
    }

    fn hello(&self) -> Hello {
        Hello {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            // Zero means the whole source is visible, which it is: it was copied into the guest in
            // one piece. A dataset opened with `Runtime::open_windowed` says a real number here,
            // and no decoder changes between the two.
            window_bytes: 0,
            max_batch_rows: self.max_batch_rows,
            offered: OFFERED,
            source_bytes: self.source.len() as u64,
        }
    }
}

/// An open dataset whose bytes have stayed where they are.
///
/// The same three things a [`Dataset`] holds, plus the source itself, which is why this one is not
/// borrowed from anything and why scanning takes `&mut self`. A source is a position as well as a
/// place: reading a range moves a window, counts a request, and in general is not something two
/// scans can do to the same source at once.
pub struct Windowed {
    program: Program,
    schema: SchemaRef,
    source: Option<Box<dyn RangeSource + Send>>,
    window_bytes: u64,
    source_bytes: u64,
    rows: u64,
    name: String,
    digest: Digest,
    max_batch_rows: u64,
    last_scan: Traffic,
}

/// Written out rather than derived, because a source is not required to be printable and requiring
/// it would be this crate deciding what a fourth implementation of the trait has to look like.
impl std::fmt::Debug for Windowed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Windowed")
            .field("name", &self.name)
            .field("rows", &self.rows)
            .field("schema", &self.schema)
            .field("window_bytes", &self.window_bytes)
            .field("source_bytes", &self.source_bytes)
            .field("max_batch_rows", &self.max_batch_rows)
            .field("last_scan", &self.last_scan)
            .field("attached", &self.source.is_some())
            .finish_non_exhaustive()
    }
}

impl Windowed {
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

    /// How much of the source the host will keep visible at once, or zero if it is not bounded.
    ///
    /// This is the number the decoder is told during the handshake, and it is the whole of what a
    /// decoder needs to know about the arrangement: a decoder that never asks for more than this in
    /// one range will never be refused for asking too much.
    #[must_use]
    pub const fn window_bytes(&self) -> u64 {
        self.window_bytes
    }

    /// How long the data section is.
    #[must_use]
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }

    /// The identity of the decoder that ran, which is the hash of its bytes.
    ///
    /// This is what the container promised and what iris-trust checked before anything compiled it,
    /// so it names the code rather than naming what the code calls itself. Two datasets that report
    /// the same digest ran the same decoder, whatever they were opened from and wherever their bytes
    /// happened to be, which is the only way a caller can say that from the outside.
    #[must_use]
    pub const fn decoder_digest(&self) -> Digest {
        self.digest
    }

    /// What the last scan cost, in requests to the source and bytes brought back.
    ///
    /// Wall clock hides the mechanism. A scan that declared four ranges and one that read the file
    /// end to end can take the same time on a warm page cache and are not the same thing at all,
    /// and the difference only shows up on a machine where the bytes are somewhere else. This is
    /// the pair of numbers that says which one happened.
    ///
    /// It covers the scan and nothing else. Opening a dataset reads a trailer, a header, a footer
    /// and the decoder module, and that traffic belongs to opening. Before any scan has run this
    /// is zero. See [`Windowed::traffic`] for the total since the source was opened.
    #[must_use]
    pub const fn last_scan(&self) -> Traffic {
        self.last_scan
    }

    /// What the source has done since it was opened, including opening it.
    ///
    /// The counters underneath only ever go up, so a caller measuring something other than one
    /// scan takes a reading either side of it and subtracts. That is what [`Traffic::since`] is
    /// for, and it is what [`Windowed::last_scan`] does internally.
    ///
    /// Zero if the source is not attached, which happens only if a scan panicked while it held it.
    #[must_use]
    pub fn traffic(&self) -> Traffic {
        self.source
            .as_ref()
            .map_or(Traffic::NONE, RangeSource::traffic)
    }

    /// Reads every row.
    ///
    /// # Errors
    ///
    /// See [`Windowed::scan_rows`].
    pub fn scan(&mut self) -> Result<Vec<RecordBatch>> {
        self.scan_rows(0, self.rows)
    }

    /// Reads a range of rows, pulling the bytes it needs as it goes.
    ///
    /// # Errors
    ///
    /// The same as [`Dataset::scan_rows`], plus [`Error::Vm`] carrying [`iris_vm::Error::Source`]
    /// if a range the decoder asked for could not be served.
    pub fn scan_rows(&mut self, start: u64, count: u64) -> Result<Vec<RecordBatch>> {
        let mut decoder = Decoder::instantiate(&self.program)?;

        // Nothing is loaded up front, so the guest's resident buffer stays empty and every range
        // the decoder asks for goes out through `require_range`. The source comes back afterwards
        // whether or not the scan worked, because a failed scan is not a reason to lose the file.
        let source = self.source.take().ok_or(Error::SourceLost)?;

        // Read before the source goes into the guest and again after it comes back, so what is
        // recorded is this scan and not everything since the file was opened. Opening read a
        // trailer, a header, a footer and a decoder module, and a caller asking what a scan cost
        // should not be handed the cost of getting to the point where a scan was possible.
        let before = source.traffic();
        decoder.attach(source);
        let outcome = run(&mut decoder, &self.hello(), &self.schema, start, count);
        self.source = decoder.detach();

        // Recorded whether or not the scan worked. A scan that failed part way through still moved
        // whatever it moved, and that is the number somebody looking at the failure wants.
        if let Some(source) = self.source.as_ref() {
            self.last_scan = source.traffic().since(before);
        }
        outcome
    }

    fn hello(&self) -> Hello {
        Hello {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            window_bytes: self.window_bytes,
            max_batch_rows: self.max_batch_rows,
            offered: OFFERED_WINDOWED,
            source_bytes: self.source_bytes,
        }
    }
}

/// Shakes hands, scans, and turns what comes back into record batches.
///
/// Both open paths end here, and the only thing that differs between them is the [`Hello`] they
/// bring. That is the claim M4 makes, written as one function rather than as a sentence: a decoder
/// handed a resident buffer and the same decoder pulling ranges out of a file it cannot hold are
/// running the same host code.
fn run(
    decoder: &mut Decoder,
    hello: &Hello,
    schema: &SchemaRef,
    start: u64,
    count: u64,
) -> Result<Vec<RecordBatch>> {
    // Waited on rather than polled. Waiting is what a host with a thread to spare does, and it is
    // what this one is: it has been handed a scan and has nothing else to do until it answers. A
    // host that does have something else to do drives `iris_vm::Running` itself, which is why the
    // suspension is in that crate rather than hidden in here.
    let handshake = decoder.start(&record(|w| hello.encode(w))?).wait()?;

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
        negotiate(hello, &ack).map_err(|refusal| Error::refused(&refusal))?;

    let request = ScanRequest {
        row_start: start,
        row_count: count,
        ..ScanRequest::everything()
    };
    let raw = decoder.scan(&record(|w| request.encode(w))?).wait()?;

    let mut batches = Vec::with_capacity(raw.len());
    for batch in &raw {
        // An empty batch is how a decoder says there are no more rows. It has no arrays, so
        // there is nothing to assemble and nothing to check against the schema.
        if batch.rows == 0 && batch.nodes.is_empty() {
            continue;
        }
        batches.push(record_batch(schema, batch)?);
    }
    Ok(batches)
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
