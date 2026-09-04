//! One running decoder, and the four calls a host makes into it.

use iris_abi::{Message, Reader};
use wasmtime::{Caller, Extern, Instance as WasmInstance, Linker, Memory, Store, TypedFunc};

use crate::batch::RawBatch;
use crate::error::{Error, Result};
use crate::module::Program;

/// What the host keeps alongside a running module.
///
/// The batches live here rather than being returned from the import because a WebAssembly import
/// returns a number, and the thing being returned is a list of buffers.
#[derive(Default)]
struct State {
    memory: Option<Memory>,
    batches: Vec<RawBatch>,
    failure: Option<Error>,
}

/// A running decoder.
///
/// The conversation is four calls and one import, all of them described in `docs/ABI.md`. Nothing in
/// this type knows what a schema is or what Arrow is. It moves records in, moves records out, and
/// copies the buffers a batch points at while the guest is still stopped inside the call that
/// produced them.
pub struct Decoder {
    store: Store<State>,
    source: TypedFunc<u32, u32>,
    input: TypedFunc<u32, u32>,
    start: TypedFunc<(), u64>,
    scan: TypedFunc<(), u64>,
    memory: Memory,
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decoder").finish_non_exhaustive()
    }
}

impl Decoder {
    /// Instantiates a compiled decoder and wires up the one import it has.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotADecoder`] if the module is missing an export the ABI requires, and
    /// [`Error::Trap`] if the module's own start code fails.
    pub fn instantiate(program: &Program) -> Result<Self> {
        let mut store = Store::new(program.engine(), State::default());
        let mut linker = Linker::new(program.engine());
        linker
            .func_wrap("iris", "emit", emit)
            .map_err(|err| trapped(&err))?;

        let instance = linker
            .instantiate(&mut store, program.module())
            .map_err(|err| trapped(&err))?;

        let memory = memory_of(&instance, &mut store)?;
        store.data_mut().memory = Some(memory);

        Ok(Self {
            source: typed(&instance, &mut store, "iris_source")?,
            input: typed(&instance, &mut store, "iris_input")?,
            start: typed(&instance, &mut store, "iris_start")?,
            scan: typed(&instance, &mut store, "iris_scan")?,
            memory,
            store,
        })
    }

    /// Hands the decoder the whole source.
    ///
    /// This is the M1 shape and it is the one thing here that does not survive contact with a large
    /// dataset. The host asks the guest for a buffer and copies the source into it, which means the
    /// source has to fit in the guest's address space and has to be copied once. Both of those go
    /// away at M4, and neither of them changes any decoder, because a decoder only ever sees the
    /// range calls the SDK makes on its behalf.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Trap`] if the guest cannot allocate the buffer, and [`Error::OutOfBounds`]
    /// if the source is larger than a `u32`.
    pub fn load_source(&mut self, bytes: &[u8]) -> Result<()> {
        let ptr = self.allocate(&self.source.clone(), bytes.len())?;
        self.write(ptr, bytes)
    }

    /// Sends the host's `Hello` and reads the answer back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] if the decoder declined, and [`Error::NotADecoder`] if it answered
    /// with something that is not a `HelloAck`.
    pub fn start(&mut self, hello: &[u8]) -> Result<Handshake> {
        let answer = self.call(&self.start.clone(), hello)?;
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
    }

    /// Runs one scan and hands back every batch it produced.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] if the decoder declined the request or declined partway through,
    /// which is why the batches are dropped on the error path: a scan that stopped early has
    /// produced a prefix of an answer, and a prefix of an answer is worse than no answer.
    pub fn scan(&mut self, request: &[u8]) -> Result<Vec<RawBatch>> {
        self.store.data_mut().batches.clear();
        self.store.data_mut().failure = None;

        let answer = self.call(&self.scan.clone(), request)?;
        if let Some(err) = self.store.data_mut().failure.take() {
            return Err(err);
        }
        if !answer.is_empty() {
            let mut reader = Reader::new(&answer);
            return match reader.message()? {
                Message::Refusal(refusal) => Err(Error::refused(&refusal)),
                _ => Err(Error::NotADecoder(
                    "the module answered a scan with something that is not a Refusal".to_owned(),
                )),
            };
        }
        Ok(core::mem::take(&mut self.store.data_mut().batches))
    }

    fn call(&mut self, func: &TypedFunc<(), u64>, record: &[u8]) -> Result<Vec<u8>> {
        let ptr = self.allocate(&self.input.clone(), record.len())?;
        self.write(ptr, record)?;
        let packed = func
            .call(&mut self.store, ())
            .map_err(|err| trapped(&err))?;
        self.read_packed(packed)
    }

    fn allocate(&mut self, func: &TypedFunc<u32, u32>, len: usize) -> Result<u32> {
        let len = u32::try_from(len)
            .map_err(|_| Error::OutOfBounds("a buffer larger than a wasm32 guest can address"))?;
        func.call(&mut self.store, len).map_err(|err| trapped(&err))
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

/// The one import a decoder has.
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
fn trapped(err: &wasmtime::Error) -> Error {
    Error::Trap(format!("{err:#}"))
}
