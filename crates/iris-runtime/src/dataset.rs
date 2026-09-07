//! Opening a container and pulling batches out of it.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use iris_abi::{
    ABI_MAJOR, ABI_MINOR, Agreement, Capability, CapabilitySet, Hello, HelloAck, Projection,
    Refusal, RefusalReason, ScanRequest, Writer, negotiate,
};
use iris_format::layout::HEADER_SIZE;
use iris_format::{Container, Digest, Directory, Placement, SchemaEncoding, Section, SectionKind};
use iris_native::{Native, Registry, Substitution};
use iris_source::{Fetch, RangeSource, Segment, SourceError, Traffic, bounds, read_blocking};
use iris_trust::{Policy, Verified};
use iris_vm::{Decoder, Handshake, Program, RawBatch, Vm};

use crate::assemble::record_batch;
use crate::error::{Error, Result};
use crate::pool::Pool;
use crate::schema::{describe, schema_from_ipc};

/// The largest record this host will build for a decoder.
///
/// A `Hello` and a `ScanRequest` are both small and neither grows with the data, so this is a bound
/// on a mistake rather than a bound on a workload.
const RECORD_LIMIT: usize = 1 << 20;

/// How much decoder this host keeps compiled at once, by default.
///
/// Weighed in module bytes and not in compiled bytes, which is a proxy and is worth being plain
/// about. Wasmtime does not offer the size of what it produced without serialising it, and
/// serialising a module in order to find out how big it is costs more than the number is worth. What
/// a module compiles to is some multiple of what it was, the multiple is a property of the engine
/// rather than of the decoder, so weighing the input orders entries the same way weighing the output
/// would and the budget is in the wrong units by a constant.
///
/// Thirty two mebibytes of module is a lot of decoders. One is tens to hundreds of kibibytes, so
/// this is a bound on a host that opens containers written by hundreds of different decoders rather
/// than a limit anything ordinary reaches. A host that knows better sets its own with
/// [`Runtime::with_decoder_cache_bytes`], and the honest way to pick the number is to measure the
/// process rather than to derive it from this one.
const DEFAULT_DECODER_CACHE: usize = 32 * 1024 * 1024;

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
///
/// Public because a native kernel has to be proved under the terms it will be run under, and these
/// are the terms. See [`iris_native::Differential::offering`].
pub const RESIDENT_TERMS: CapabilitySet = CapabilitySet::new()
    .with(Capability::RANDOM_ACCESS)
    .with(Capability::PROJECTION);

/// What this host can do when the container stays where it is.
///
/// Everything the resident path offers, plus the two bits that describe pulling. `require-range`
/// says the decoder may ask for bytes it was not given, and `sliding-window` says it will not be
/// given all of them at once, which is the same thing the non zero `window_bytes` in the handshake
/// says and is here so that a decoder can refuse on a bit rather than on a number.
///
/// Public for the same reason [`RESIDENT_TERMS`] is. A kernel proved under one of the two is
/// substituted on that open path and not on the other, since the other one is terms nobody compared
/// it under.
pub const WINDOWED_TERMS: CapabilitySet = RESIDENT_TERMS
    .with(Capability::REQUIRE_RANGE)
    .with(Capability::SLIDING_WINDOW);

/// A compiler, the decoders it has already compiled, and the terms this host offers one.
///
/// One of these is meant to be shared and reused, and sharing one is worth more than it looks.
/// It holds the Wasmtime engine, which is expensive to build and caches across every module it
/// compiles, and it holds the compiled decoders themselves, which is what stops a query that opens
/// the same container from eight partitions compiling the same code eight times. Cloning it shares
/// both: a clone is a handle to the same engine and the same pool, not a second copy of either.
#[derive(Clone, Debug)]
pub struct Runtime {
    vm: Vm,
    max_batch_rows: u64,
    policy: Policy,
    /// Compiled decoders, keyed by the hash of the bytes they were compiled from.
    ///
    /// The digest is the whole key and it is enough to be the whole key. It was checked against the
    /// module before anything compiled it, so two containers that name the same digest carry the
    /// same bytes and there is no version of this where they compile to different code. What is not
    /// in the key is the deadline, because that is not a property of the compiled module: a program
    /// that comes out of here is restamped with this runtime's deadline on the way past.
    ///
    /// Nothing is shared between two runtimes built separately, and that is not an oversight. A
    /// compiled module belongs to the engine that compiled it and cannot be instantiated by another
    /// one, so a pool that outlived an engine would be holding code nothing could run.
    decoders: Arc<Pool<Digest, Program>>,
    /// Decoders this host has its own implementation of, keyed by the digest of the module.
    ///
    /// Empty by default, which is every container running in the sandbox. What goes in it is a
    /// decision an operator makes, in the same way that allowing a decoder from outside the
    /// container is, and for the same reason: both of them are about running code that did not come
    /// with the dataset.
    native: Registry,
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
            decoders: Arc::new(Pool::new(DEFAULT_DECODER_CACHE)),
            native: Registry::new(),
        })
    }

    /// Says which decoders this host has its own implementation of.
    ///
    /// A decoder in a container is WebAssembly and runs in a sandbox, which is what makes a dataset
    /// from anywhere readable. It also caps the vector width at 128 bits, so a host that has written
    /// the same decoder against the machine it is actually running on has something faster and no
    /// way to use it. This is that way.
    ///
    /// The registry is keyed on the digest of the decoder module and on nothing else. The digest
    /// used to look one up is the one `iris-trust` computed from the module bytes that were present
    /// in the container, not a name and not anything the container asserts about itself, so a
    /// dataset cannot reach a native implementation by claiming to be something it is not. A decoder
    /// whose digest is not in the registry runs in the sandbox whatever it calls itself. See
    /// [`iris_native::Registry`] for why that is the only key.
    ///
    /// Substitution replaces compiling and running the module. It replaces nothing else: the module
    /// is still hashed and checked, the handshake is still negotiated by the same function, and
    /// every batch still goes through `iris-guard` and Arrow. [`Dataset::decoder_is_native`] is how
    /// a host checks from the outside that this happened.
    #[must_use]
    pub fn with_native(mut self, native: Registry) -> Self {
        self.native = native;
        self
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

    /// Sets how much decoder this host keeps compiled at once.
    ///
    /// Compiled decoders are memory, and a host that keeps every one it has ever seen has a leak
    /// with a cache in front of it. The number is in module bytes rather than compiled bytes, for
    /// the reason given where the default is defined, and zero is the way to say no sharing at all:
    /// nothing fits in a budget of nothing, so every open compiles.
    ///
    /// This starts a fresh and empty pool rather than resizing the one in hand, so a clone taken
    /// before it keeps the pool it was cloned with. That is the same rule
    /// [`Runtime::with_decoder_deadline`] follows, and it is what makes a runtime something a host
    /// configures once and then shares rather than something two threads can reconfigure underneath
    /// each other.
    #[must_use]
    pub fn with_decoder_cache_bytes(mut self, bytes: usize) -> Self {
        self.decoders = Arc::new(Pool::new(bytes));
        self
    }

    /// How many decoders this runtime has compiled since the pool it holds was made.
    ///
    /// The number that says whether sharing is working. Eight partitions opening one container move
    /// it by one, and a host that sees it climbing with the query count is a host whose runtime is
    /// being rebuilt rather than shared.
    #[must_use]
    pub fn decoders_compiled(&self) -> u64 {
        self.decoders.builds()
    }

    /// How many compiled decoders are held right now.
    #[must_use]
    pub fn decoders_cached(&self) -> usize {
        self.decoders.entries()
    }

    /// How much of the budget those decoders are using.
    ///
    /// In the same module bytes the budget is set in, which is a proxy for the compiled code and is
    /// described where the default is defined. A host watching this sit at the budget is a host that
    /// is throwing decoders away and compiling them again, which is worth either more budget or
    /// fewer decoders.
    #[must_use]
    pub fn decoders_cached_bytes(&self) -> usize {
        self.decoders.held()
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
        let opened = self.prepare(container.directory(), &verified, RESIDENT_TERMS)?;
        let source = container.section_bytes(data_section(container.directory())?);

        Ok(Dataset {
            chosen: opened.chosen,
            schema: opened.schema,
            source,
            rows: opened.rows,
            name: opened.name,
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
        let opened = self.prepare(&directory, &verified, WINDOWED_TERMS)?;

        let section = data_section(&directory)?;
        let (at, len) = (section.offset, section.len);
        // Everything below borrows nothing from the footer, so the metadata can go now and the
        // handle that is left is a program, a schema and a source.
        let window_bytes = source.largest().unwrap_or(0) as u64;
        let data = Segment::new(source, at, len)?;

        Ok(Windowed {
            chosen: opened.chosen,
            schema: opened.schema,
            source: Some(Box::new(data)),
            window_bytes,
            source_bytes: len,
            rows: opened.rows,
            name: opened.name,
            max_batch_rows: self.max_batch_rows,
            last_scan: Traffic::NONE,
        })
    }

    /// Everything the two paths do between having the metadata and having a compiled decoder.
    ///
    /// It is one function because the checks and the order they happen in are the interesting part,
    /// and two copies of an ordering is two orderings waiting to drift.
    fn prepare(
        &self,
        directory: &Directory<'_>,
        verified: &Verified<'_>,
        offered: CapabilitySet,
    ) -> Result<Opened> {
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
        //
        // It is also the key, and the pool is what stops a scan split across partitions compiling
        // one decoder once per partition. A hit hands back a module somebody else compiled, which is
        // sound for exactly the reason the digest is worth having: these bytes hashed to this value
        // and were checked against the container before the first compile, so a second container
        // naming the same digest carries the same code.
        //
        // The deadline is put on afterwards rather than being part of the key. It is what this
        // runtime is willing to wait for and not a property of the compiled module, so two runtimes
        // that disagree about it can still share the compiler's work.
        let digest = verified.digest();

        // The one place a native implementation is chosen, and the digest above is the whole of what
        // chooses it. It is worth being clear about which digest that is: it came from iris-trust,
        // which computed it from the module bytes that were actually in the container and compared
        // it against what the container claims. A dataset that names a digest this host has native
        // code for, while carrying a module that hashes to something else, was refused several lines
        // ago. So there is no arrangement of bytes that reaches this branch by asserting anything.
        //
        // The name never appears. A registry keyed on the name would hand native code to any dataset
        // that typed the right string, which is arbitrary code selection by filename, and the reason
        // that cannot happen here is that `Registry` has no method that takes one.
        //
        // The terms this path offers are part of the lookup, because a kernel is proved under terms
        // and not outright. A kernel that was never compared against the module with projection on
        // offer is not substituted on a path that offers projection, and the container goes to the
        // sandbox instead. That is the direction to fail in: adding a capability to what this host
        // offers costs substitution until somebody runs the differential again.
        let decoding = match self.native.get(&digest, offered) {
            Some(substitution) => Decoding::Native(substitution),
            None => Decoding::Wasm(
                self.decoders
                    .get_or_build(&digest, verified.module().len(), || {
                        self.vm.compile(verified.module(), &digest.to_string())
                    })?
                    .with_deadline(self.vm.deadline()),
            ),
        };

        Ok(Opened {
            chosen: Chosen { digest, decoding },
            schema,
            rows: directory.dataset().rows,
            name: directory.dataset().name.clone(),
        })
    }
}

/// What both open paths have once the metadata has been read and the decoder is ready to run.
struct Opened {
    chosen: Chosen,
    schema: SchemaRef,
    rows: u64,
    name: String,
}

/// Which implementation is going to read the rows, and the digest that chose it.
///
/// The two travel together because every scan has to write both of them down, and a pair of
/// arguments is a pair somebody can hand the digest of one decoder and the implementation of
/// another. There is one place they are put together, which is the lookup that used the digest to
/// pick the implementation, and after that they move as one thing.
#[derive(Clone, Debug)]
struct Chosen {
    digest: Digest,
    decoding: Decoding,
}

/// Which code is going to read this container's rows.
///
/// Both arms have already been through the same checks by the time one of them is chosen. The
/// module was hashed and compared against the container either way, because the hash is what picks
/// the arm, and everything the two produce is checked the same way afterwards. What differs is only
/// who does the decoding.
#[derive(Clone, Debug)]
enum Decoding {
    /// The module the container carries, compiled and run in the sandbox. The default and the
    /// answer for every decoder this host has not been told about.
    Wasm(Program),

    /// Host code registered against the digest of that module, run in place of it.
    ///
    /// The lookup hands back the implementation and the proof it was admitted on together, rather
    /// than the implementation on its own, because a scan has to be able to say afterwards which one
    /// it ran. See [`note`], which is the only reader of the second half.
    Native(Substitution),
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
    chosen: Chosen,
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

    /// The identity of the decoder that ran, which is the hash of its bytes.
    ///
    /// This is what the container promised and what iris-trust checked before anything compiled it,
    /// so it names the code rather than naming what the code calls itself. Two datasets that report
    /// the same digest ran the same decoder, whatever they were opened from and wherever their bytes
    /// happened to be, which is the only way a caller can say that from the outside.
    #[must_use]
    pub const fn decoder_digest(&self) -> Digest {
        self.chosen.digest
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
        self.scan_rows_columns(start, count, &[])
    }

    /// Reads every row of the columns named, in the order they are named.
    ///
    /// # Errors
    ///
    /// See [`Dataset::scan_rows_columns`].
    pub fn scan_columns(&self, columns: &[u32]) -> Result<Vec<RecordBatch>> {
        self.scan_rows_columns(0, self.rows, columns)
    }

    /// Reads a range of rows of the columns named, in the order they are named.
    ///
    /// The batches that come back carry the projected schema rather than the container's, so a
    /// caller reading the third field of a batch gets the third column it asked for and not the
    /// third column of the dataset.
    ///
    /// Nothing is saved on this path. The source is already resident and the whole of it was
    /// already paid for, so a projection here is the decoder doing less work rather than the host
    /// moving fewer bytes. [`Windowed::scan_rows_columns`] is where it costs less.
    ///
    /// # Errors
    ///
    /// The same as [`Dataset::scan_rows`], plus [`Error::Projection`] if a column named is not one
    /// the dataset has, and [`Error::Refused`] if the decoder did not agree to
    /// [`Capability::PROJECTION`] and so cannot be asked for part of a row.
    pub fn scan_rows_columns(
        &self,
        start: u64,
        count: u64,
        columns: &[u32],
    ) -> Result<Vec<RecordBatch>> {
        let outcome = self.run(start, count, columns);
        note(&self.chosen, start, count, columns, &outcome);
        outcome
    }

    /// The scan itself, with the line about it written by the caller above.
    ///
    /// Split out so there is one place the outcome exists before it is returned. A scan that logged
    /// on the way out of each arm would be two lines to keep saying the same thing, and the failing
    /// paths would be the ones that got missed.
    fn run(&self, start: u64, count: u64, columns: &[u32]) -> Result<Vec<RecordBatch>> {
        match &self.chosen.decoding {
            Decoding::Wasm(program) => {
                let mut decoder = Decoder::instantiate(program)?;
                decoder.load_source(self.source)?;
                blocking(run_wasm(
                    &mut decoder,
                    &self.hello(),
                    &self.schema,
                    start,
                    count,
                    columns,
                ))
            }
            // Nothing is copied here, which the sandbox path cannot say. Loading the source into a
            // guest means putting a second copy of the data section inside the guest's memory,
            // because that is the only place guest code can address. Native code is running in this
            // process and can read the buffer where it already is, so it is handed a source over the
            // same bytes rather than a copy of them.
            Decoding::Native(substitution) => {
                let mut source = Resident { bytes: self.source };
                blocking(run_native(
                    substitution.native(),
                    &mut source,
                    &self.hello(),
                    &self.schema,
                    start,
                    count,
                    columns,
                ))
            }
        }
    }

    /// What this host and the decoder settled on, without reading a row.
    ///
    /// A caller planning a scan needs this before it plans one. Projection is the case that matters:
    /// asking a decoder that never agreed to [`Capability::PROJECTION`] for three columns out of
    /// forty is refused rather than served whole, so a query engine deciding whether to push a
    /// projection down or to apply it itself has to be able to ask first. Everything else in the set
    /// is worth the same kind of question, which is why this hands back the set rather than an
    /// answer about one bit.
    ///
    /// It costs one instantiation and one handshake, and no scan. That is a fraction of what
    /// compiling the module cost, and this is meant to be asked once when a dataset is opened rather
    /// than once per scan.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] if the decoder and this host cannot agree on terms at all, and
    /// [`Error::Vm`] if the decoder trapped during the handshake.
    pub fn capabilities(&self) -> Result<CapabilitySet> {
        match &self.chosen.decoding {
            Decoding::Wasm(program) => {
                let mut decoder = Decoder::instantiate(program)?;
                decoder.load_source(self.source)?;
                Ok(blocking(agree_wasm(&mut decoder, &self.hello()))?.agreed)
            }
            Decoding::Native(substitution) => {
                Ok(agree_native(substitution.native(), &self.hello())?.agreed)
            }
        }
    }

    /// Whether this host ran its own implementation instead of the module in the container.
    ///
    /// True only if the digest of that module was in the registry handed to
    /// [`Runtime::with_native`]. There is no other way for this to be true, which is what makes it
    /// worth asking: a host that means to be running native code can check that it is, and a host
    /// that does not want to can check that it is not.
    ///
    /// The rows do not depend on the answer. A native implementation that disagrees with the module
    /// it stands in for is a bug in that implementation, and this is how somebody chasing one finds
    /// out which of the two ran.
    #[must_use]
    pub const fn decoder_is_native(&self) -> bool {
        matches!(self.chosen.decoding, Decoding::Native(_))
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
            offered: RESIDENT_TERMS,
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
    chosen: Chosen,
    schema: SchemaRef,
    source: Option<Box<dyn RangeSource + Send>>,
    window_bytes: u64,
    source_bytes: u64,
    rows: u64,
    name: String,
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
            .field("native", &self.decoder_is_native())
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
        self.chosen.digest
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

    /// Reads every row, without holding the thread while the bytes are on their way.
    ///
    /// # Errors
    ///
    /// See [`Windowed::scan_rows`].
    pub async fn scan_async(&mut self) -> Result<Vec<RecordBatch>> {
        self.scan_rows_columns_async(0, self.rows, &[]).await
    }

    /// Reads a range of rows, pulling the bytes it needs as it goes.
    ///
    /// # Errors
    ///
    /// The same as [`Dataset::scan_rows`], plus [`Error::Vm`] carrying [`iris_vm::Error::Source`]
    /// if a range the decoder asked for could not be served.
    pub fn scan_rows(&mut self, start: u64, count: u64) -> Result<Vec<RecordBatch>> {
        self.scan_rows_columns(start, count, &[])
    }

    /// Reads a range of rows, without holding the thread while the bytes are on their way.
    ///
    /// # Errors
    ///
    /// See [`Windowed::scan_rows`].
    pub async fn scan_rows_async(&mut self, start: u64, count: u64) -> Result<Vec<RecordBatch>> {
        self.scan_rows_columns_async(start, count, &[]).await
    }

    /// Reads every row of the columns named, in the order they are named.
    ///
    /// # Errors
    ///
    /// See [`Windowed::scan_rows_columns`].
    pub fn scan_columns(&mut self, columns: &[u32]) -> Result<Vec<RecordBatch>> {
        self.scan_rows_columns(0, self.rows, columns)
    }

    /// Reads the columns named, without holding the thread while the bytes are on their way.
    ///
    /// # Errors
    ///
    /// See [`Windowed::scan_rows_columns`].
    pub async fn scan_columns_async(&mut self, columns: &[u32]) -> Result<Vec<RecordBatch>> {
        self.scan_rows_columns_async(0, self.rows, columns).await
    }

    /// Reads a range of rows of the columns named, pulling only the bytes those columns need.
    ///
    /// This is the path where a projection is worth something. The host does not decide which bytes
    /// to fetch, the decoder does, by naming ranges, so a projection that reaches storage is one
    /// the decoder was told about and acted on. [`Windowed::last_scan`] is where that shows up: a
    /// scan of three columns out of forty over a real network moves about three fortieths of the
    /// data section, and if it moves all of it then the projection was a filter applied after the
    /// fact and the pushdown did not happen.
    ///
    /// A decoder is free to fetch more than the projection strictly needs, and a well behaved one
    /// does when the alternative is many small requests. What it must not do is fetch the columns
    /// nobody asked for.
    ///
    /// # Errors
    ///
    /// The same as [`Windowed::scan_rows`], plus [`Error::Projection`] if a column named is not one
    /// the dataset has, and [`Error::Refused`] if the decoder did not agree to
    /// [`Capability::PROJECTION`] and so cannot be asked for part of a row.
    pub fn scan_rows_columns(
        &mut self,
        start: u64,
        count: u64,
        columns: &[u32],
    ) -> Result<Vec<RecordBatch>> {
        blocking(self.scan_rows_columns_async(start, count, columns))
    }

    /// The same scan, on a thread the caller wants back while the bytes are on their way.
    ///
    /// This is the shape a query engine wants and [`Windowed::scan_rows_columns`] is this one driven
    /// by a thread that has agreed to sit there. A decoder that asks for a range the source does not
    /// have yet suspends, and this future goes pending with it, so the executor runs something else
    /// on that worker instead of spending a core on a network round trip. A source that knows when
    /// its bytes will land wakes the task, and one that does not is asked again straight away, which
    /// still gives the worker up between tries.
    ///
    /// Nothing about the scan differs. It is the same decoder, the same ranges, the same batches and
    /// the same traffic counters, which is why there is one body here and not two.
    ///
    /// # Errors
    ///
    /// The same as [`Windowed::scan_rows_columns`].
    pub async fn scan_rows_columns_async(
        &mut self,
        start: u64,
        count: u64,
        columns: &[u32],
    ) -> Result<Vec<RecordBatch>> {
        // The source comes back afterwards whether or not the scan worked, because a failed scan is
        // not a reason to lose the file.
        let source = self.source.take().ok_or(Error::SourceLost)?;

        // Read before the source is handed over and again after it comes back, so what is recorded
        // is this scan and not everything since the file was opened. Opening read a trailer, a
        // header, a footer and a decoder module, and a caller asking what a scan cost should not be
        // handed the cost of getting to the point where a scan was possible.
        let before = source.traffic();
        let (outcome, back) = decode(
            &self.chosen,
            source,
            &self.hello(),
            &self.schema,
            start,
            count,
            columns,
        )
        .await;
        self.source = back;

        // Recorded whether or not the scan worked. A scan that failed part way through still moved
        // whatever it moved, and that is the number somebody looking at the failure wants.
        if let Some(source) = self.source.as_ref() {
            self.last_scan = source.traffic().since(before);
        }
        outcome
    }

    /// What this host and the decoder settled on, without reading a row.
    ///
    /// See [`Dataset::capabilities`], which answers the same question about the other path. This
    /// one takes the source out and puts it back the way a scan does, so it is not free: the
    /// handshake itself asks nothing of the source, but a source that was lost to a panicking scan
    /// is reported here as [`Error::SourceLost`] rather than as an empty set.
    ///
    /// # Errors
    ///
    /// The same as [`Dataset::capabilities`], plus [`Error::SourceLost`] if the source is not
    /// attached.
    pub fn capabilities(&mut self) -> Result<CapabilitySet> {
        match &self.chosen.decoding {
            Decoding::Wasm(program) => {
                let mut decoder = Decoder::instantiate(program)?;
                let source = self.source.take().ok_or(Error::SourceLost)?;
                decoder.attach(source);
                let agreed = blocking(agree_wasm(&mut decoder, &self.hello()));
                self.source = decoder.detach();
                Ok(agreed?.agreed)
            }
            // The source is not taken here, and it is checked for anyway. A native implementation
            // shakes hands without reading a byte, so this could answer with the source still
            // attached, and then a dataset whose source was lost to a panicking scan would report
            // capabilities happily and fail at the first scan. Answering the same way on both paths
            // is worth more than saving a check.
            Decoding::Native(substitution) => {
                if self.source.is_none() {
                    return Err(Error::SourceLost);
                }
                Ok(agree_native(substitution.native(), &self.hello())?.agreed)
            }
        }
    }

    /// Whether this host ran its own implementation instead of the module in the container.
    ///
    /// See [`Dataset::decoder_is_native`], which answers the same question about the other path.
    #[must_use]
    pub const fn decoder_is_native(&self) -> bool {
        matches!(self.chosen.decoding, Decoding::Native(_))
    }

    fn hello(&self) -> Hello {
        Hello {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            window_bytes: self.window_bytes,
            max_batch_rows: self.max_batch_rows,
            offered: WINDOWED_TERMS,
            source_bytes: self.source_bytes,
        }
    }
}

/// Runs a scan to the end on a thread that has agreed to sit there.
///
/// Every synchronous entry point in this module is one of the asynchronous ones driven by this, so
/// there is one implementation of a scan and two ways to ask for it rather than two implementations
/// that have to be kept saying the same thing.
///
/// The waker is the one that does nothing, which is the honest description of what this is: the
/// thread is the scheduler, it has nothing else to run, and it comes back and asks again. A source
/// handed this waker sees `wake_when_ready` succeed and then fires something that goes nowhere,
/// which costs nothing and is why that method is allowed to be optimistic. Yielding between tries
/// rather than spinning tight is what keeps a source whose fetch runs on another thread of the same
/// pool from being starved by the thread waiting for it.
fn blocking<T>(call: impl Future<Output = T>) -> T {
    let mut call = pin!(call);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(answer) = call.as_mut().poll(&mut cx) {
            return answer;
        }
        std::thread::yield_now();
    }
}

/// Runs a windowed scan on whichever decoder the dataset opened with, and gives the source back.
///
/// The source is passed in and returned rather than borrowed, because the sandbox path does not
/// borrow it: a source goes into the guest's store for the duration of a scan and comes out again
/// afterwards. Returning it alongside the outcome is what makes losing it on a failure impossible to
/// write, since there is no way out of here that does not carry it.
///
/// It takes the four things it needs rather than a `&Windowed`, which is not a style preference. A
/// future holding a `&Windowed` across an await is `Send` only if `Windowed` is `Sync`, and a
/// `Windowed` owns a `Box<dyn RangeSource + Send>` and is deliberately not `Sync`, because a source
/// is a position and two threads reading one at once is not a thing any of the implementations
/// promise to survive. So a scan future that borrowed the dataset could not be spawned, which is the
/// whole point of there being an asynchronous scan.
async fn decode(
    chosen: &Chosen,
    source: Box<dyn RangeSource + Send>,
    hello: &Hello,
    schema: &SchemaRef,
    start: u64,
    count: u64,
    columns: &[u32],
) -> (
    Result<Vec<RecordBatch>>,
    Option<Box<dyn RangeSource + Send>>,
) {
    let (outcome, back) = match &chosen.decoding {
        Decoding::Wasm(program) => match Decoder::instantiate(program) {
            Err(err) => (Err(err.into()), Some(source)),
            Ok(mut decoder) => {
                // Nothing is loaded up front, so the guest's resident buffer stays empty and every
                // range the decoder asks for goes out through `require_range`.
                decoder.attach(source);
                let outcome = run_wasm(&mut decoder, hello, schema, start, count, columns).await;
                (outcome, decoder.detach())
            }
        },
        Decoding::Native(substitution) => {
            let mut source = source;
            let outcome = run_native(
                substitution.native(),
                source.as_mut(),
                hello,
                schema,
                start,
                count,
                columns,
            )
            .await;
            (outcome, Some(source))
        }
    };
    note(chosen, start, count, columns, &outcome);
    (outcome, back)
}

/// Shakes hands with the decoder in the container, scans, and turns what comes back into batches.
///
/// Both open paths end here, and the only thing that differs between them is the [`Hello`] they
/// bring. That is the claim M4 makes, written as one function rather than as a sentence: a decoder
/// handed a resident buffer and the same decoder pulling ranges out of a file it cannot hold are
/// running the same host code.
async fn run_wasm(
    decoder: &mut Decoder,
    hello: &Hello,
    schema: &SchemaRef,
    start: u64,
    count: u64,
    columns: &[u32],
) -> Result<Vec<RecordBatch>> {
    let agreement = agree_wasm(decoder, hello).await?;
    let plan = plan(schema, agreement.agreed, columns)?;
    let request = plan.request(start, count)?;
    let raw = decoder
        .scan(&record(|w| request.encode(w))?)
        .finish()
        .await?;
    assemble(&plan.projected, &raw)
}

/// The same scan, run by host code registered against this decoder's digest.
///
/// Everything either side of the decoding is the function the sandbox path calls. The terms are
/// negotiated by `negotiate`, the projection is checked by [`plan`], and the batches are checked by
/// [`assemble`], which is `iris-guard` and then Arrow. That is the point of writing it this way
/// rather than letting a native implementation return record batches directly: a native kernel is
/// substituted for a decoder, not for the host, and it is held to the same account.
async fn run_native(
    native: &dyn Native,
    source: &mut (dyn RangeSource + Send),
    hello: &Hello,
    schema: &SchemaRef,
    start: u64,
    count: u64,
    columns: &[u32],
) -> Result<Vec<RecordBatch>> {
    let agreement = agree_native(native, hello)?;
    let plan = plan(schema, agreement.agreed, columns)?;
    let request = plan.request(start, count)?;
    let raw = native.scan(hello, &request, source).await?;
    assemble(&plan.projected, &raw)
}

/// Writes down which of the two implementations ran, and what it was.
///
/// One event per scan, on both paths, whether the scan worked or not. The question it answers is the
/// first one anybody asks when a number is wrong: did this come out of the sandbox or out of native
/// code. That is not answerable from the batches, because agreeing on the batches is the whole point
/// of substitution, and it is not answerable from a return value either, since both paths return the
/// same type. So it has to be written down at the time.
///
/// Three identities and they are three different things. `decoder` is the digest `iris-trust`
/// computed from the module bytes in the container, which names the decoder the dataset shipped
/// whichever implementation ended up reading it. `kernel` is the digest of the differential run that
/// admitted the substitute, so two hosts that log the same value ran the same implementation against
/// the same corpus under the same terms. `implementation` is what that code calls itself, which is
/// the one an operator can act on directly and the one nothing verifies. On the sandbox path the
/// last two are absent, which is itself the answer.
///
/// At debug rather than at info, because it is per scan and a scan can be a millisecond. A host that
/// wants it in production turns it on for `iris::scan` and gets this without the rest of the crate's
/// debug output.
///
/// The failure goes in the same event rather than a second one. A scan that failed still ran on one
/// of the two paths, and that is exactly the case where somebody wants to know which.
fn note(
    chosen: &Chosen,
    start: u64,
    count: u64,
    columns: &[u32],
    outcome: &Result<Vec<RecordBatch>>,
) {
    let (path, kernel, implementation) = match &chosen.decoding {
        Decoding::Wasm(_) => ("sandbox", None, None),
        Decoding::Native(substitution) => (
            "native",
            Some(substitution.proof()),
            Some(substitution.identity()),
        ),
    };
    tracing::debug!(
        target: "iris::scan",
        path,
        decoder = %chosen.digest,
        kernel = kernel.map(tracing::field::display),
        implementation,
        row_start = start,
        row_count = count,
        columns = ?columns,
        batches = outcome.as_ref().map_or(0, Vec::len),
        failed = outcome.as_ref().err().map(tracing::field::display),
        "scanned",
    );
}

/// What a scan settles before it asks for a row.
struct Plan {
    /// The schema the batches will carry, which is the projected one.
    projected: SchemaRef,
    /// The columns wanted, encoded the way the ABI wants them.
    indices: Vec<u8>,
}

impl Plan {
    /// The request to send, which is the same request whichever side is going to serve it.
    fn request(&self, start: u64, count: u64) -> Result<ScanRequest<'_>> {
        Ok(ScanRequest {
            row_start: start,
            row_count: count,
            projection: Projection::from_bytes(&self.indices)?,
            ..ScanRequest::everything()
        })
    }
}

/// Checks that the scan being asked for is one the decoder agreed to serve, and works out its shape.
///
/// The projection check is before anything is asked for rather than after something comes back. A
/// decoder that never mentioned projection will read every column whatever it is sent, and the
/// batches it produces would then be assembled against a schema of three fields with forty arrays in
/// hand. That is a `Shape` error naming a count, which sends whoever reads it looking for a bug in
/// the decoder rather than at the one line of theirs that asked for something the decoder cannot do.
fn plan(schema: &SchemaRef, agreed: CapabilitySet, columns: &[u32]) -> Result<Plan> {
    if !columns.is_empty() && !agreed.contains(Capability::PROJECTION) {
        return Err(Error::refused(&Refusal::new(
            RefusalReason::MISSING_CAPABILITY,
            "this scan names columns and the decoder did not agree to projection, so it would              read every column and the batches would not match what was asked for",
        )));
    }
    Ok(Plan {
        projected: project(schema, columns)?,
        indices: columns.iter().flat_map(|c| c.to_le_bytes()).collect(),
    })
}

/// Turns what a decoder produced into record batches, checking every one of them on the way.
///
/// One function for both paths, and it is the one that matters most for being shared. A native
/// implementation is host code with no sandbox around it, so the temptation is to trust its output
/// because it is ours, and the answer to that is that the arrays it produced are still described by
/// numbers and the numbers are still checked. `record_batch` is the guard and then Arrow, and it is
/// what runs here.
fn assemble(projected: &SchemaRef, raw: &[RawBatch]) -> Result<Vec<RecordBatch>> {
    let mut batches = Vec::with_capacity(raw.len());
    for batch in raw {
        // An empty batch is how a decoder says there are no more rows. It has no arrays, so
        // there is nothing to assemble and nothing to check against the schema.
        if batch.rows == 0 && batch.nodes.is_empty() {
            continue;
        }
        batches.push(record_batch(projected, batch)?);
    }
    Ok(batches)
}

/// Shakes hands with a decoder in the sandbox, and hands back what the two sides settled on.
///
/// Awaited rather than waited on, even though a handshake asks nothing of the source, because a
/// decoder is allowed to read a footer in order to know its own shape and this is a call into guest
/// code like any other. A decoder that does that on a source over a network would otherwise hold a
/// worker for a round trip before the scan had started.
async fn agree_wasm(decoder: &mut Decoder, hello: &Hello) -> Result<Agreement> {
    let handshake = decoder
        .start(&record(|w| hello.encode(w))?)
        .finish()
        .await?;
    agree(hello, &handshake)
}

/// Shakes hands with a native implementation.
///
/// Not asynchronous, because there is no guest to park and nothing to read: a native implementation
/// answers out of what it already knows. It goes through the same [`agree`] afterwards, so an
/// implementation that claims a capability this host did not offer is refused in exactly the way a
/// guest making the same claim would be.
fn agree_native(native: &dyn Native, hello: &Hello) -> Result<Agreement> {
    agree(hello, &native.handshake(hello)?)
}

/// The negotiation, which is the same on both paths because it is about the terms and not about who
/// is going to honour them.
///
/// The decoder has already said yes by the time the ack is built, and this is the host saying yes
/// back. Both sides check, because a decoder that agrees to terms it cannot meet and a host that
/// runs a decoder it cannot serve are different bugs and only one of them is ours.
fn agree(hello: &Hello, handshake: &Handshake) -> Result<Agreement> {
    let ack = HelloAck {
        abi_major: handshake.abi_major,
        abi_minor: handshake.abi_minor,
        required: handshake.required,
        optional: handshake.optional,
        decoder_id: &handshake.decoder_id,
    };
    negotiate(hello, &ack).map_err(|refusal| Error::refused(&refusal))
}

/// A [`RangeSource`] over bytes this host is already holding.
///
/// `iris_source::MemorySource` is the same idea and takes `Bytes`, which means copying the data
/// section in order to hand a native implementation a view of bytes it could already see. The whole
/// reason the resident path is the fast one is that nothing is copied, so it gets its own dozen
/// lines instead.
///
/// Nothing here is ever pending and nothing here counts, both for the reason `MemorySource` gives:
/// whoever produced this buffer paid for it before iris was handed the result.
struct Resident<'a> {
    bytes: &'a [u8],
}

impl RangeSource for Resident<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn range(&mut self, at: u64, len: usize) -> std::result::Result<Fetch<'_>, SourceError> {
        bounds(at, len, self.len())?;

        // The bounds check passed, so `at` is at most the buffer length and both conversions fit.
        let start = usize::try_from(at).unwrap_or(usize::MAX);
        Ok(Fetch::Ready(&self.bytes[start..start + len]))
    }

    fn traffic(&self) -> Traffic {
        Traffic::NONE
    }
}

/// The schema the batches of a projected scan carry.
///
/// An empty projection means every column, here and in the ABI both, so it hands the container's
/// own schema back untouched. Anything else is the named fields in the order they were named, which
/// is the order the decoder emits its arrays in, so the two cannot drift apart.
///
/// The bounds check is here rather than left to Arrow because the message is the whole value of it.
/// A caller that asked for column forty of a dataset with forty columns has made an off by one, and
/// a sentence saying which index and how many there are ends that in one read.
fn project(schema: &SchemaRef, columns: &[u32]) -> Result<SchemaRef> {
    if columns.is_empty() {
        return Ok(SchemaRef::clone(schema));
    }
    let fields = schema.fields().len();
    let mut indices = Vec::with_capacity(columns.len());
    for &column in columns {
        let at = usize::try_from(column).unwrap_or(usize::MAX);
        if at >= fields {
            return Err(Error::Projection { column, fields });
        }
        indices.push(at);
    }
    Ok(SchemaRef::new(schema.project(&indices)?))
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
