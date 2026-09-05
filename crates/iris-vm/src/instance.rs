//! One running decoder, and the calls a host makes into it.

use std::time::Duration;

use iris_abi::{Message, RangeStatus, Reader};
use iris_source::{Fetch, RangeSource, SourceError};
use wasmtime::{
    AsContextMut, Caller, Extern, Instance as WasmInstance, Linker, Memory, Store, Trap, TypedFunc,
};

use crate::batch::RawBatch;
use crate::error::{Error, Result};
use crate::module::Program;
use crate::run::{Running, Yield, settled};

/// What the host keeps alongside a running module.
///
/// The batches live here rather than being returned from the import because a WebAssembly import
/// returns a number, and the thing being returned is a list of buffers. The source lives here for a
/// different reason: the import that serves ranges is handed a `Caller` and nothing else, so
/// whatever it is going to read from has to be reachable through the store.
#[derive(Default)]
struct State {
    memory: Option<Memory>,
    batches: Vec<RawBatch>,
    failure: Option<Error>,
    source: Option<Box<dyn RangeSource + Send>>,
    /// The per-call budget, in epoch ticks, kept here so that serving a range can put it back.
    ticks: u64,
}

/// A running decoder.
///
/// The conversation is four calls and two imports, all of them described in `docs/ABI.md`. Nothing
/// in this type knows what a schema is or what Arrow is. It moves records in, moves records out, and
/// copies the buffers a batch points at while the guest is still stopped inside the call that
/// produced them.
pub struct Decoder {
    store: Store<State>,
    source: TypedFunc<u32, u32>,
    input: TypedFunc<u32, u32>,
    start: TypedFunc<(), u64>,
    scan: TypedFunc<(), u64>,
    memory: Memory,
    identity: String,
    deadline: Duration,
    ticks: u64,
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decoder").finish_non_exhaustive()
    }
}

impl Decoder {
    /// Instantiates a compiled decoder and wires up the imports it has.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotADecoder`] if the module is missing an export the ABI requires,
    /// [`Error::Trap`] if the module's own start code fails, and [`Error::Deadline`] if that start
    /// code does not come back.
    pub fn instantiate(program: &Program) -> Result<Self> {
        let decoder = program.decoder();
        let deadline = program.deadline();
        let ticks = program.ticks();

        let mut store = Store::new(
            program.engine(),
            State {
                ticks,
                ..State::default()
            },
        );

        // Armed before the module is instantiated rather than after, because instantiating runs
        // whatever the module put in its start function, and that is guest code like any other.
        store.set_epoch_deadline(ticks);

        let mut linker = Linker::new(program.engine());
        linker
            .func_wrap("iris", "emit", emit)
            .map_err(|err| trapped(&err, decoder, deadline))?;
        linker
            .func_wrap_async("iris", "require_range", require_range)
            .map_err(|err| trapped(&err, decoder, deadline))?;

        // Instantiating goes through the asynchronous door because one of the two imports can
        // suspend, and Wasmtime makes that a property of the store rather than of the call. Nothing
        // here can actually park, which is what `settled` says.
        let instance = settled(linker.instantiate_async(&mut store, program.module()))
            .map_err(|err| trapped(&err, decoder, deadline))?;

        let memory = memory_of(&instance, &mut store)?;
        store.data_mut().memory = Some(memory);

        Ok(Self {
            source: typed(&instance, &mut store, "iris_source")?,
            input: typed(&instance, &mut store, "iris_input")?,
            start: typed(&instance, &mut store, "iris_start")?,
            scan: typed(&instance, &mut store, "iris_scan")?,
            memory,
            store,
            identity: decoder.to_owned(),
            deadline,
            ticks,
        })
    }

    /// Gives the decoder somewhere to pull ranges from.
    ///
    /// This is the other half of [`Decoder::load_source`] and hosts pick one. Loading copies the
    /// whole source in up front and suits a small file that is already in memory. Attaching leaves
    /// the bytes where they are and lets the decoder ask for the parts it wants, which is the only
    /// one of the two that works when the source is larger than the guest can address.
    ///
    /// A decoder that asks for a range with nothing attached is told [`RangeStatus::NO_SOURCE`],
    /// which is a host bug rather than a decoder bug and says so.
    pub fn attach(&mut self, source: Box<dyn RangeSource + Send>) {
        self.store.data_mut().source = Some(source);
    }

    /// Takes the source back.
    ///
    /// Worth having because a source counts things. How many requests a scan made and how many bytes
    /// came back are properties of the source, and a host that handed one over still wants to read
    /// them afterwards.
    pub fn detach(&mut self) -> Option<Box<dyn RangeSource + Send>> {
        self.store.data_mut().source.take()
    }

    /// Hands the decoder the whole source.
    ///
    /// The M1 shape, and the one thing here that does not survive contact with a large dataset. The
    /// host asks the guest for a buffer and copies the source into it, which means the source has to
    /// fit in the guest's address space and has to be copied once. [`Decoder::attach`] is what
    /// replaces it, and neither changes any decoder, because a decoder only ever sees the range
    /// calls the SDK makes on its behalf.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Trap`] if the guest cannot allocate the buffer, and [`Error::OutOfBounds`]
    /// if the source is larger than a `u32`.
    pub fn load_source(&mut self, bytes: &[u8]) -> Result<()> {
        let source = self.source.clone();
        let ptr = self.allocate(&source, bytes.len())?;
        self.write(ptr, bytes)
    }

    /// Sends the host's `Hello` and reads the answer back.
    ///
    /// Opening is a call into the guest like any other, so a decoder that reads a footer in order to
    /// know its own shape can suspend here just as it can in the middle of a scan.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] if the decoder declined, and [`Error::NotADecoder`] if it answered
    /// with something that is not a `HelloAck`.
    pub fn start<'a>(&'a mut self, hello: &'a [u8]) -> Running<'a, Handshake> {
        let start = self.start.clone();
        Running::new(async move {
            let answer = self.call(&start, hello).await?;
            let mut reader = Reader::new(&answer);
            match reader.message()? {
                Message::HelloAck(ack) => Ok(Handshake {
                    abi_major: ack.abi_major,
                    abi_minor: ack.abi_minor,
                    required: ack.required,
                    optional: ack.optional,
                    decoder_id: ack.decoder_id.to_owned(),
                }),
                Message::Refusal(refusal) => Err(Error::refused(&refusal)),
                _ => Err(Error::NotADecoder(
                    "the module answered a Hello with something that is not a HelloAck".to_owned(),
                )),
            }
        })
    }

    /// Runs one scan and hands back every batch it produced.
    ///
    /// The call may suspend any number of times before it finishes. See [`Running`] for what that
    /// means and why the answer does not simply arrive.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] if the decoder declined the request or declined partway through,
    /// which is why the batches are dropped on the error path: a scan that stopped early has
    /// produced a prefix of an answer, and a prefix of an answer is worse than no answer.
    pub fn scan<'a>(&'a mut self, request: &'a [u8]) -> Running<'a, Vec<RawBatch>> {
        let scan = self.scan.clone();
        Running::new(async move {
            self.store.data_mut().batches.clear();
            self.store.data_mut().failure = None;

            let answer = self.call(&scan, request).await?;
            if let Some(err) = self.store.data_mut().failure.take() {
                return Err(err);
            }
            if !answer.is_empty() {
                let mut reader = Reader::new(&answer);
                return match reader.message()? {
                    Message::Refusal(refusal) => Err(Error::refused(&refusal)),
                    _ => Err(Error::NotADecoder(
                        "the module answered a scan with something that is not a Refusal"
                            .to_owned(),
                    )),
                };
            }
            Ok(core::mem::take(&mut self.store.data_mut().batches))
        })
    }

    async fn call(&mut self, func: &TypedFunc<(), u64>, record: &[u8]) -> Result<Vec<u8>> {
        let input = self.input.clone();
        let ptr = self.allocate(&input, record.len())?;
        self.write(ptr, record)?;
        self.arm();
        let packed = func
            .call_async(&mut self.store, ())
            .await
            .map_err(|err| trapped(&err, &self.identity, self.deadline))?;
        self.read_packed(packed)
    }

    fn allocate(&mut self, func: &TypedFunc<u32, u32>, len: usize) -> Result<u32> {
        let len = u32::try_from(len)
            .map_err(|_| Error::OutOfBounds("a buffer larger than a wasm32 guest can address"))?;
        self.arm();
        settled(func.call_async(&mut self.store, len))
            .map_err(|err| trapped(&err, &self.identity, self.deadline))
    }

    /// Gives the guest a fresh budget for the call that is about to happen.
    ///
    /// The budget is per call rather than per instance, because a call is where the host gets
    /// control back and is therefore the only place the question "has this taken too long" has an
    /// answer a host can act on. A decoder that spends its whole budget on every call and returns
    /// is slow rather than hostile, and a slow decoder is a problem for whoever chose it.
    ///
    /// A suspension re-arms it too, in [`require_range`]. Time spent waiting on a range is the
    /// host's own I/O and charging the decoder for it would mean a slow disk looking exactly like a
    /// decoder that will not stop.
    fn arm(&mut self) {
        self.store.set_epoch_deadline(self.ticks);
    }

    fn write(&mut self, ptr: u32, bytes: &[u8]) -> Result<()> {
        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .map_err(|_| Error::OutOfBounds("the guest asked the host to write outside its memory"))
    }

    fn read_packed(&mut self, packed: u64) -> Result<Vec<u8>> {
        if packed == 0 {
            return Ok(Vec::new());
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the two halves of a packed answer are u32 by construction"
        )]
        let (ptr, len) = ((packed >> 32) as u32, packed as u32);
        let data = self.memory.data(&self.store);
        slice(data, ptr, len).map(<[u8]>::to_vec)
    }
}

/// What the decoder said about itself when it opened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Handshake {
    /// The ABI major version the decoder was built against.
    pub abi_major: u16,
    /// The ABI minor version the decoder was built against.
    pub abi_minor: u16,
    /// What the decoder cannot run without.
    pub required: iris_abi::CapabilitySet,
    /// What the decoder will use if it is there.
    pub optional: iris_abi::CapabilitySet,
    /// The decoder's name for itself, which nothing interprets.
    pub decoder_id: String,
}

/// The import a decoder asks for bytes through.
///
/// This is the range inversion, in one function. The decoder names bytes and a buffer of its own to
/// put them in, and everything about how those bytes are obtained stays on this side: which file is
/// open, what is cached, how many requests are in flight, what happens on a timeout.
///
/// It is asynchronous because a source is allowed to say "not yet". When that happens the guest is
/// parked mid instruction with its stack intact and the host thread goes back to whoever polled the
/// call. Nothing is replayed when it resumes, so a scan that misses on every one of its ranges costs
/// one suspension per range rather than one restart per range.
fn require_range(
    mut caller: Caller<'_, State>,
    (offset, len, dst): (u64, u32, u32),
) -> Box<dyn Future<Output = u32> + Send + '_> {
    Box::new(async move {
        // Taken out of the store for the duration of the call. Serving a range needs the source and
        // the guest's memory at the same time, and those are two fields of one struct, so borrowing
        // the struct twice is not allowed and moving one of them out is the honest way to say that
        // they are independent.
        let Some(mut source) = caller.data_mut().source.take() else {
            caller.data_mut().failure = Some(Error::NoSource);
            return RangeStatus::NO_SOURCE.0;
        };
        let status = serve(&mut caller, source.as_mut(), offset, len, dst).await;
        caller.data_mut().source = Some(source);
        status.0
    })
}

/// Waits for a range, however many times it takes, and copies it into the guest.
async fn serve(
    caller: &mut Caller<'_, State>,
    source: &mut (dyn RangeSource + Send),
    offset: u64,
    len: u32,
    dst: u32,
) -> RangeStatus {
    let want = len as usize;

    // Readiness is established without keeping the bytes, and then one more call hands them back.
    // The borrow the loop takes cannot be seen to end by a borrow checker that has not been told
    // the iteration returned, and the second call is a comparison against a range that was just
    // made ready. `iris-source` does the same thing in `read_blocking` for the same reason.
    loop {
        match source.range(offset, want) {
            Ok(fetch) if fetch.is_ready() => break,
            Ok(_) => {
                // The budget goes back before the guest is parked. What it is there to catch is a
                // decoder that will not return, and a decoder waiting on a range is not running at
                // all, so charging it for the wait would turn a slow object store into a decoder
                // that looks hostile.
                let ticks = caller.data().ticks;
                caller.as_context_mut().set_epoch_deadline(ticks);
                Yield::once().await;
            }
            Err(err) => return refuse(caller, &err),
        }
    }

    match source.range(offset, want) {
        Ok(Fetch::Ready(bytes)) => write_range(caller, dst, bytes),
        Ok(_) => refuse(
            caller,
            &SourceError::Flapped {
                at: offset,
                end: offset.saturating_add(u64::from(len)),
            },
        ),
        Err(err) => refuse(caller, &err),
    }
}

/// Copies a range the source produced into the buffer the guest named.
fn write_range(caller: &mut Caller<'_, State>, dst: u32, bytes: &[u8]) -> RangeStatus {
    let Some(memory) = caller.data().memory else {
        caller.data_mut().failure = Some(Error::NotADecoder(
            "the module asked for a range before its memory was wired up".to_owned(),
        ));
        return RangeStatus::UNAVAILABLE;
    };

    if memory.write(&mut *caller, dst as usize, bytes).is_err() {
        caller.data_mut().failure = Some(Error::OutOfBounds(
            "the guest asked for a range to be written outside its memory",
        ));
        return RangeStatus::UNAVAILABLE;
    }
    RangeStatus::SERVED
}

/// Turns a source's complaint into the number the guest sees, and decides whether the scan is over.
///
/// The split is between a decoder that asked for the wrong thing and a host that could not answer a
/// reasonable question. The first two are answers: a decoder told its range is out of bounds or does
/// not fit in one request can ask differently, and the scan carries on. The rest end the scan,
/// because nothing the decoder does next will make the bytes appear and a decoder that keeps going
/// is producing an answer from data it never received.
fn refuse(caller: &mut Caller<'_, State>, error: &SourceError) -> RangeStatus {
    match error {
        SourceError::OutOfBounds { .. } => RangeStatus::OUT_OF_BOUNDS,
        SourceError::TooLarge { .. } => RangeStatus::TOO_LARGE,
        other => {
            caller.data_mut().failure = Some(Error::Source(other.to_string()));
            RangeStatus::UNAVAILABLE
        }
    }
}

/// The import a batch record leaves the guest through.
///
/// Everything a batch points at is copied here, because here is the only place it is certainly
/// still there. The guest is stopped inside this call, so nothing can reuse a buffer underneath the
/// host, and the moment the call returns that stops being true.
fn emit(mut caller: Caller<'_, State>, ptr: u32, len: u32) -> u32 {
    let Some(memory) = caller.data().memory else {
        caller.data_mut().failure = Some(Error::NotADecoder(
            "the module emitted a batch before its memory was wired up".to_owned(),
        ));
        return 1;
    };

    let outcome = {
        let data = memory.data(&caller);
        slice(data, ptr, len).and_then(|record| collect(data, record))
    };

    match outcome {
        Ok(batch) => {
            caller.data_mut().batches.push(batch);
            0
        }
        Err(err) => {
            caller.data_mut().failure = Some(err);
            1
        }
    }
}

/// Reads a batch record and copies out every buffer it names.
fn collect(memory: &[u8], record: &[u8]) -> Result<RawBatch> {
    let mut reader = Reader::new(record);
    let Message::Batch(batch) = reader.message()? else {
        return Err(Error::NotADecoder(
            "the module emitted something that is not a batch".to_owned(),
        ));
    };

    // No count field is read here and none exists. The number of buffers is however many the record
    // was long enough to describe, so a batch cannot make the host reserve a gigabyte by claiming
    // it is about to send one.
    let mut buffers = Vec::with_capacity(batch.buffers.len());
    for buffer in batch.buffers.iter() {
        let offset = u32::try_from(buffer.offset).map_err(|_| {
            Error::OutOfBounds("a batch buffer starts past what a wasm32 guest can address")
        })?;
        let len = u32::try_from(buffer.len).map_err(|_| {
            Error::OutOfBounds("a batch buffer is longer than a wasm32 guest can address")
        })?;
        buffers.push(slice(memory, offset, len)?.to_vec());
    }

    Ok(RawBatch {
        rows: batch.rows,
        nodes: batch.nodes.iter().collect(),
        buffers,
    })
}

/// Bounds checks an address and a length against the guest's memory.
fn slice(memory: &[u8], ptr: u32, len: u32) -> Result<&[u8]> {
    let start = ptr as usize;
    let end = start
        .checked_add(len as usize)
        .ok_or(Error::OutOfBounds("an address and a length overflow"))?;
    memory.get(start..end).ok_or(Error::OutOfBounds(
        "a range runs off the end of guest memory",
    ))
}

fn memory_of(instance: &WasmInstance, store: &mut Store<State>) -> Result<Memory> {
    match instance.get_export(&mut *store, "memory") {
        Some(Extern::Memory(memory)) => Ok(memory),
        _ => Err(Error::NotADecoder(
            "the module does not export its memory, so the host cannot hand it anything".to_owned(),
        )),
    }
}

fn typed<P, R>(
    instance: &WasmInstance,
    store: &mut Store<State>,
    name: &str,
) -> Result<TypedFunc<P, R>>
where
    P: wasmtime::WasmParams,
    R: wasmtime::WasmResults,
{
    instance
        .get_typed_func(&mut *store, name)
        .map_err(|err| Error::NotADecoder(format!("{name}: {err}")))
}

/// Turns whatever the engine said into a message, which is all this crate lets out.
///
/// A deadline is told apart from every other trap here, because the two mean different things to
/// whoever reads the log. A trap is a decoder that broke. A deadline is a decoder that would still
/// be running, and the number worth knowing about it is how long it was given.
fn trapped(err: &wasmtime::Error, decoder: &str, deadline: Duration) -> Error {
    if matches!(err.downcast_ref::<Trap>(), Some(&Trap::Interrupt)) {
        return Error::Deadline {
            decoder: decoder.to_owned(),
            limit: deadline,
        };
    }
    Error::Trap {
        decoder: decoder.to_owned(),
        detail: format!("{err:#}"),
    }
}
