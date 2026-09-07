//! Proving that a native kernel and the module it stands in for produce the same bytes.
//!
//! Registering a native implementation is an assertion, and the assertion is a strong one: this
//! host code produces, for every dataset that module reads, exactly what running that module would
//! have produced. An unverified assertion of that kind is worse than having no fast path at all,
//! because the fast path is the one that runs and the slow path is the one everybody read.
//!
//! So there is no way to make a [`Kernel`] except by running one of these, and there is no way to
//! put anything but a [`Kernel`] in a [`Registry`](crate::Registry). Code that registers a native
//! implementation without a differential run does not compile, which is a stronger place to catch it
//! than a build script or a test: a build script can be skipped and a test can be filtered out, and
//! neither of those is available against a type that has no other constructor.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use iris_abi::{
    ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet, Hello, HelloAck, Projection, ScanRequest,
    Writer, negotiate,
};
use iris_format::Digest;
use iris_source::MemorySource;
use iris_vm::{Decoder, Handshake, Program, RawBatch, Vm};

use crate::corpus::{Case, Corpus};
use crate::native::Native;

/// The largest record this harness will build for a decoder.
///
/// A `Hello` and a `ScanRequest` are both small and neither grows with the data, so this bounds a
/// mistake rather than a workload. It is the same bound `iris-runtime` puts on the same two records,
/// and the two are separate because this crate sits below that one.
const RECORD_LIMIT: usize = 1 << 20;

/// A native implementation that has been proved against the module it stands in for.
///
/// The only thing that makes one of these is [`Differential::verify`], and the only thing that takes
/// one is [`Registry::with`](crate::Registry::with). That is the whole of the enforcement, and it is
/// worth saying plainly that it is enforcement of a run having happened rather than of the run being
/// thorough. What the run covers is the corpus it was given, and `Corpus` says what that is worth.
#[derive(Clone, Debug)]
pub struct Kernel {
    digest: Digest,
    native: Arc<dyn Native>,
    offered: Vec<CapabilitySet>,
}

impl Kernel {
    /// The digest of the module this implementation was proved against.
    ///
    /// This is the key it goes in the registry under, and it is derived from the module bytes rather
    /// than typed by anybody, so there is no version of a registration that names the wrong module
    /// and then never fires.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// The terms this implementation was proved under.
    ///
    /// A host offering anything else gets the sandbox, which is the point of recording them. See
    /// [`Differential::offering`].
    #[must_use]
    pub fn offered(&self) -> &[CapabilitySet] {
        &self.offered
    }

    /// Takes the parts apart for the registry.
    pub(crate) fn into_parts(self) -> (Digest, Arc<dyn Native>, Vec<CapabilitySet>) {
        (self.digest, self.native, self.offered)
    }

    /// A kernel that never ran anything, for the tests in this crate that are about the table.
    ///
    /// Only exists while this crate's own tests are being compiled, so nothing outside can reach it
    /// and no amount of enthusiasm can turn it into an escape hatch. The runs that use real modules
    /// live in `iris-runtime`'s tests, because that is where a decoder gets built.
    #[cfg(test)]
    pub(crate) fn untested(
        digest: Digest,
        native: Arc<dyn Native>,
        offered: Vec<CapabilitySet>,
    ) -> Self {
        Self {
            digest,
            native,
            offered,
        }
    }
}

/// A differential run waiting to be told what to compare.
///
/// # What it runs
///
/// Every case in the corpus, under every set of terms it was told about, at two batch sizes, over a
/// derived set of requests: the whole dataset, a row from each end, the second half, an empty
/// request, a request that starts past the last row, and one request per column if the two sides
/// agreed to projection. Both implementations are handed the same bytes and asked the same thing,
/// and every batch that comes back has to match: the same number of batches, the same row counts,
/// the same nodes, the same buffer lengths and the same bytes in them.
///
/// The batching has to match as well as the values, which is worth being explicit about because it
/// is a stronger claim than "the same rows". The list of batches is what a scan hands back, so an
/// implementation that returns one batch where the module returned eight has changed what a caller
/// sees even though the rows add up. That is why the batch size is part of what is run rather than
/// left at a default: a kernel that ignores `max_batch_rows` passes at one value and fails at the
/// other.
///
/// # What it does not prove
///
/// That the kernel is right about anything the corpus did not ask it. A corpus is a set of datasets
/// somebody chose, and no run over it can say what happens on the dataset nobody thought of. There
/// are two things standing behind it. The batches a native implementation emits go through
/// `iris-guard` and then Arrow on every scan, exactly as a guest's do, so an implementation that
/// gets a length wrong outside the corpus is refused rather than believed. And the terms are part of
/// the proof: a kernel proved under terms that did not include projection is not substituted on a
/// host that offers projection, because it was never asked to project.
///
/// The `Hello` this builds says the whole source is visible, since the corpus is held in memory.
/// A decoder that changes what it emits based on the window size it was told about is not covered by
/// that, and would be the case to add here first.
#[derive(Debug)]
pub struct Differential<'a> {
    vm: &'a Vm,
    module: &'a [u8],
    offered: Vec<CapabilitySet>,
}

impl<'a> Differential<'a> {
    /// A run against these module bytes, compiled by this engine.
    ///
    /// The module is the one the kernel stands in for, and it is passed as bytes rather than as a
    /// digest for the same reason the registry used to take bytes: the identity is then derived from
    /// what is actually there instead of copied out of somewhere by hand.
    #[must_use]
    pub const fn new(vm: &'a Vm, module: &'a [u8]) -> Self {
        Self {
            vm,
            module,
            offered: Vec::new(),
        }
    }

    /// Adds a set of terms to verify under.
    ///
    /// A host offers a decoder a capability set, and what a decoder does depends on it, so a proof
    /// is a proof under terms rather than a proof outright. `iris-runtime` publishes the two sets it
    /// offers as `RESIDENT_TERMS` and `WINDOWED_TERMS`, and naming both is what gets a kernel
    /// substituted on both open paths.
    ///
    /// At least one is required. A run with no terms would negotiate nothing and compare nothing,
    /// and it comes back as [`Mismatch::NoTerms`] rather than as a pass.
    #[must_use]
    pub fn offering(mut self, offered: CapabilitySet) -> Self {
        if !self.offered.contains(&offered) {
            self.offered.push(offered);
        }
        self
    }

    /// Runs the comparison and, if everything matched, hands back the thing a registry accepts.
    ///
    /// # Errors
    ///
    /// A [`Mismatch`] naming the case, the request and the byte. The messages are written for
    /// somebody who has just been told their kernel disagrees with the module and has to go and find
    /// out where, so a difference in the values is reported as the first offset that differs rather
    /// than as two buffers.
    pub fn verify(self, native: Arc<dyn Native>, corpus: &Corpus) -> Result<Kernel, Mismatch> {
        self.run(native.as_ref(), corpus)?;
        Ok(Kernel {
            digest: Digest::of(self.module),
            native,
            offered: self.offered,
        })
    }

    /// The run itself, split out so that building the kernel is the last thing that happens.
    fn run(&self, native: &dyn Native, corpus: &Corpus) -> Result<(), Mismatch> {
        if self.offered.is_empty() {
            return Err(Mismatch::NoTerms);
        }
        if corpus.is_empty() {
            return Err(Mismatch::EmptyCorpus);
        }
        for case in corpus.cases() {
            if case.rows() == 0 {
                return Err(Mismatch::EmptyCase {
                    case: case.name().to_owned(),
                });
            }
        }

        let digest = Digest::of(self.module);
        let program = self
            .vm
            .compile(self.module, &digest.to_string())
            .map_err(|err| Mismatch::Compile(err.to_string()))?;

        for case in corpus.cases() {
            let mut served = 0usize;
            for &terms in &self.offered {
                for batch_rows in batch_sizes(case.rows()) {
                    served += run_case(&program, native, case, terms, batch_rows)?;
                }
            }
            if served == 0 {
                return Err(Mismatch::Nothing {
                    case: case.name().to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// One case, under one set of terms, at one batch size.
///
/// Returns how many of the derived requests the module served, which is what tells the caller
/// whether the case proved anything at all.
fn run_case(
    program: &Program,
    native: &dyn Native,
    case: &Case,
    terms: CapabilitySet,
    batch_rows: u64,
) -> Result<usize, Mismatch> {
    let named = || case.name().to_owned();
    let hello = greeting(case, terms, batch_rows);
    let label = format!("batches of {batch_rows}, {}", how(terms));

    let opened = |detail| Mismatch::Module {
        case: case.name().to_owned(),
        detail,
    };
    let (_, module_shake) = open_module(program, &hello, case, terms).map_err(opened)?;
    let native_shake = native
        .handshake(&hello)
        .map_err(|err| Mismatch::Handshake {
            case: named(),
            detail: format!("the native implementation refused the handshake: {err}"),
        })?;
    if module_shake != native_shake {
        return Err(Mismatch::Handshake {
            case: named(),
            detail: format!(
                "the module said {module_shake:?} and the native implementation said {native_shake:?}"
            ),
        });
    }

    let ack = HelloAck {
        abi_major: module_shake.abi_major,
        abi_minor: module_shake.abi_minor,
        required: module_shake.required,
        optional: module_shake.optional,
        decoder_id: &module_shake.decoder_id,
    };
    let agreement = negotiate(&hello, &ack).map_err(|refusal| Mismatch::Terms {
        detail: format!("{}, under {}", refusal.detail, how(terms)),
    })?;

    let mut served = 0usize;
    for ask in asks(case, agreement.agreed) {
        let ask_label = format!("{}, {label}", ask.what);

        // A fresh instance per request, because that is what the host does. A decoder that carried
        // state from one scan into the next would be a different decoder on the second one, and
        // reusing an instance here would hide that rather than find it.
        let (mut decoder, _) = open_module(program, &hello, case, terms).map_err(opened)?;
        let from_module = scan_module(&mut decoder, &ask);
        let from_native = scan_native(native, &hello, case, &ask);

        match (from_module, from_native) {
            (Ok(module), Ok(native)) => {
                served += 1;
                compare(case.name(), &ask_label, &module, &native)?;
            }
            // Both refused, which is agreement of the only kind there is to have about a request
            // neither of them serves. The text of two refusals is not compared, because one comes
            // out of a guest and one does not and neither is what a caller acts on.
            (Err(_), Err(_)) => {}
            (Ok(_), Err(detail)) => {
                return Err(Mismatch::Outcome {
                    case: named(),
                    ask: ask_label,
                    detail: format!(
                        "the module served this and the native implementation would not: {detail}"
                    ),
                });
            }
            (Err(detail), Ok(_)) => {
                return Err(Mismatch::Outcome {
                    case: named(),
                    ask: ask_label,
                    detail: format!(
                        "the native implementation served this and the module would not: {detail}"
                    ),
                });
            }
        }
    }
    Ok(served)
}

/// The two batch sizes every case is run at.
///
/// One that puts the whole dataset in a single batch and one that forces the decoder to chunk. A
/// kernel that ignores the number it was given passes the first and fails the second, which is the
/// point of there being two. They collapse to one when the dataset is small enough that a third of
/// it is the whole of it, and running the same thing twice is not worth the seconds.
fn batch_sizes(rows: u64) -> Vec<u64> {
    let whole = rows.max(1);
    let chunked = (rows / 3).max(1);
    if whole == chunked {
        vec![whole]
    } else {
        vec![whole, chunked]
    }
}

/// What the host says to the decoder for this case.
fn greeting(case: &Case, terms: CapabilitySet, batch_rows: u64) -> Hello {
    let bytes = case.data().len() as u64;
    Hello {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        // The corpus is resident, so a run under terms that mention windows is telling the truth
        // when it says the window is the whole source. What is not covered is a window smaller than
        // that, which is the note in `Differential`.
        window_bytes: if terms.contains(Capability::SLIDING_WINDOW) {
            bytes
        } else {
            0
        },
        max_batch_rows: batch_rows,
        offered: terms,
        source_bytes: bytes,
    }
}

/// How the host would hand this decoder its bytes under these terms, in one word.
///
/// The two open paths differ in exactly this, so it is the label a mismatch is reported against.
fn how(terms: CapabilitySet) -> &'static str {
    if terms.contains(Capability::REQUIRE_RANGE) {
        "pulling its own ranges"
    } else {
        "reading a resident copy"
    }
}

/// One request, and what to call it when it fails.
struct Ask {
    what: String,
    row_start: u64,
    row_count: u64,
    columns: Vec<u32>,
}

impl Ask {
    fn new(what: impl Into<String>, row_start: u64, row_count: u64) -> Self {
        Self {
            what: what.into(),
            row_start,
            row_count,
            columns: Vec::new(),
        }
    }

    fn projecting(mut self, columns: Vec<u32>) -> Self {
        self.columns = columns;
        self
    }

    /// The projection, encoded the way the ABI wants it.
    fn indices(&self) -> Vec<u8> {
        self.columns.iter().flat_map(|c| c.to_le_bytes()).collect()
    }
}

/// The requests derived from a case.
///
/// Derived rather than supplied, because a kernel author choosing which requests prove their own
/// kernel is the same problem as a kernel author choosing whether to run this at all. The ends and
/// the empty request are here because that is where an implementation written from a description of
/// a format rather than from the module differs from it.
fn asks(case: &Case, agreed: CapabilitySet) -> Vec<Ask> {
    let rows = case.rows();
    let mut asks = vec![
        Ask::new("every row", 0, rows),
        Ask::new("every row, asked for as everything", 0, u64::MAX),
        Ask::new("the first row", 0, 1),
        Ask::new("the last row", rows - 1, 1),
        Ask::new("the second half", rows / 2, rows - rows / 2),
        Ask::new("no rows at all", 0, 0),
        Ask::new("one row starting past the last one", rows, 1),
    ];
    if agreed.contains(Capability::PROJECTION) {
        for column in 0..case.columns() {
            asks.push(Ask::new(format!("column {column}"), 0, rows).projecting(vec![column]));
        }
        if case.columns() >= 2 {
            let last = case.columns() - 1;
            // The order a projection names its columns is the order the arrays come back in, so a
            // kernel that treats a projection as a set rather than as a list is wrong here and
            // nowhere else.
            asks.push(Ask::new("the last column and the first", 0, rows).projecting(vec![last, 0]));
        }
    }
    asks
}

/// Instantiates the module and shakes hands with it, the way the host would under these terms.
///
/// Which of the two ways the source is handed over is decided by the terms, because that is how
/// `iris-runtime` decides it: a host that has not offered `require-range` copies the source into the
/// guest, and a host that has offered it attaches a source and serves ranges. Getting that wrong
/// here would verify a decoder against a path it is never run on.
fn open_module(
    program: &Program,
    hello: &Hello,
    case: &Case,
    terms: CapabilitySet,
) -> Result<(Decoder, Handshake), String> {
    let greeting = record(|w| hello.encode(w)).map_err(|err| err.to_string())?;
    let mut decoder = Decoder::instantiate(program).map_err(|err| err.to_string())?;
    if terms.contains(Capability::REQUIRE_RANGE) {
        decoder.attach(Box::new(MemorySource::new(case.data().to_vec())));
    } else {
        decoder
            .load_source(case.data())
            .map_err(|err| err.to_string())?;
    }
    let handshake = decoder
        .start(&greeting)
        .wait()
        .map_err(|err| err.to_string())?;
    Ok((decoder, handshake))
}

/// Asks the module for the rows, on the thread that is already committed to this.
fn scan_module(decoder: &mut Decoder, ask: &Ask) -> Result<Vec<RawBatch>, String> {
    let indices = ask.indices();
    let request = ScanRequest {
        row_start: ask.row_start,
        row_count: ask.row_count,
        projection: Projection::from_bytes(&indices).map_err(|err| err.to_string())?,
        ..ScanRequest::everything()
    };
    let encoded = record(|w| request.encode(w)).map_err(|err| err.to_string())?;
    decoder.scan(&encoded).wait().map_err(|err| err.to_string())
}

/// Asks the native implementation for the same rows, over its own copy of the same bytes.
fn scan_native(
    native: &dyn Native,
    hello: &Hello,
    case: &Case,
    ask: &Ask,
) -> Result<Vec<RawBatch>, String> {
    let indices = ask.indices();
    let request = ScanRequest {
        row_start: ask.row_start,
        row_count: ask.row_count,
        projection: Projection::from_bytes(&indices).map_err(|err| err.to_string())?,
        ..ScanRequest::everything()
    };
    let mut source = MemorySource::new(case.data().to_vec());
    settled(native.scan(hello, &request, &mut source)).map_err(|err| err.to_string())
}

/// Compares what the two sides produced, and says where they first differ.
fn compare(
    case: &str,
    ask: &str,
    module: &[RawBatch],
    native: &[RawBatch],
) -> Result<(), Mismatch> {
    if module.len() != native.len() {
        return Err(Mismatch::Batches {
            case: case.to_owned(),
            ask: ask.to_owned(),
            module: module.len(),
            native: native.len(),
        });
    }
    for (batch, (module, native)) in module.iter().zip(native).enumerate() {
        let shape = |detail: String| Mismatch::Shape {
            case: case.to_owned(),
            ask: ask.to_owned(),
            batch,
            detail,
        };
        if module.rows != native.rows {
            return Err(shape(format!(
                "the module produced {} rows and the native implementation produced {}",
                module.rows, native.rows
            )));
        }
        if module.nodes != native.nodes {
            return Err(shape(format!(
                "the module described its arrays as {:?} and the native implementation described \
                 them as {:?}",
                module.nodes, native.nodes
            )));
        }
        if module.buffers.len() != native.buffers.len() {
            return Err(shape(format!(
                "the module produced {} buffers and the native implementation produced {}",
                module.buffers.len(),
                native.buffers.len()
            )));
        }
        for (buffer, (module, native)) in module.buffers.iter().zip(&native.buffers).enumerate() {
            if module.len() != native.len() {
                return Err(shape(format!(
                    "buffer {buffer} is {} bytes from the module and {} bytes from the native \
                     implementation",
                    module.len(),
                    native.len()
                )));
            }
            if let Some(at) = module.iter().zip(native).position(|(a, b)| a != b) {
                return Err(Mismatch::Bytes {
                    case: case.to_owned(),
                    ask: ask.to_owned(),
                    batch,
                    buffer,
                    at,
                    module: module[at],
                    native: native[at],
                });
            }
        }
    }
    Ok(())
}

/// Runs a native scan on the thread that asked for it.
///
/// Nothing in a differential run can be pending, since every case is held in memory, and the loop is
/// here for the implementation that awaits something else anyway. It yields rather than spinning
/// tight, which is the same arrangement `iris-runtime` uses for its own blocking entry points.
fn settled<T>(call: impl Future<Output = T>) -> T {
    let mut call = pin!(call);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(answer) = call.as_mut().poll(&mut cx) {
            return answer;
        }
        std::thread::yield_now();
    }
}

/// Writes a record into a fresh buffer, growing until it fits.
///
/// The same dozen lines `iris-runtime` has, and it is copied rather than shared because the copy
/// there is private to a crate that sits above this one. Moving it into `iris-abi` would mean that
/// crate allocating, and it is written so that a guest with no allocator can use it.
fn record(body: impl Fn(&mut Writer<'_>) -> iris_abi::Result<()>) -> iris_abi::Result<Vec<u8>> {
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
            Err(err) => return Err(err),
        }
    }
}

/// What a differential run found, when it did not find agreement.
///
/// Every variant names the case and, where there is one, the request, because a kernel that
/// disagrees with a module disagrees somewhere specific and the whole value of running this is being
/// told where.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Mismatch {
    /// Nothing was offered, so there were no terms to negotiate under.
    #[error("a differential run needs the terms it is verifying under, and none were named")]
    NoTerms,

    /// The corpus was empty, which is a pass that proves nothing.
    #[error("a differential run over an empty corpus would prove nothing, so it is not a run")]
    EmptyCorpus,

    /// A case had no rows, which is the same problem one dataset at a time.
    #[error("the case `{case}` has no rows in it, so there is nothing to compare on it")]
    EmptyCase {
        /// Which case.
        case: String,
    },

    /// The module is not one this build can compile.
    #[error("the module this kernel stands in for does not compile: {0}")]
    Compile(String),

    /// The module would not open a case, so that case cannot be compared.
    #[error("the module could not read the case `{case}`, so the case proves nothing: {detail}")]
    Module {
        /// Which case.
        case: String,
        /// What the module said.
        detail: String,
    },

    /// The module refused every request derived from a case.
    #[error(
        "the module served none of the requests derived from `{case}`, so nothing was compared"
    )]
    Nothing {
        /// Which case.
        case: String,
    },

    /// The decoder will not run under the terms this run was told to verify under.
    #[error("the decoder refuses these terms, so nothing would ever run under them: {detail}")]
    Terms {
        /// What the refusal said.
        detail: String,
    },

    /// The two implementations described themselves differently.
    #[error("the two implementations answered the handshake differently on `{case}`: {detail}")]
    Handshake {
        /// Which case.
        case: String,
        /// How they differed.
        detail: String,
    },

    /// One side served a request and the other would not.
    #[error("on `{case}`, asked for {ask}: {detail}")]
    Outcome {
        /// Which case.
        case: String,
        /// Which request.
        ask: String,
        /// Which side did what.
        detail: String,
    },

    /// The two implementations cut the rows into a different number of batches.
    #[error(
        "on `{case}`, asked for {ask}: the module produced {module} batches and the native \
         implementation produced {native}"
    )]
    Batches {
        /// Which case.
        case: String,
        /// Which request.
        ask: String,
        /// How many the module produced.
        module: usize,
        /// How many the native implementation produced.
        native: usize,
    },

    /// A batch was described differently by the two of them.
    #[error("on `{case}`, asked for {ask}, batch {batch}: {detail}")]
    Shape {
        /// Which case.
        case: String,
        /// Which request.
        ask: String,
        /// Which batch.
        batch: usize,
        /// How they differed.
        detail: String,
    },

    /// The values differ, which is the one this whole thing exists to find.
    #[error(
        "on `{case}`, asked for {ask}, batch {batch}, buffer {buffer}: the first byte that differs \
         is at offset {at}, where the module produced {module:#04x} and the native implementation \
         produced {native:#04x}"
    )]
    Bytes {
        /// Which case.
        case: String,
        /// Which request.
        ask: String,
        /// Which batch.
        batch: usize,
        /// Which buffer of that batch.
        buffer: usize,
        /// The first offset the two disagree at.
        at: usize,
        /// What the module put there.
        module: u8,
        /// What the native implementation put there.
        native: u8,
    },
}
