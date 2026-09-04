//! Driving a decoder without a host.
//!
//! A decoder is a WebAssembly module in the end, but the decode loop is ordinary Rust and there is
//! no reason to need a runtime, a sandbox and a container in order to test it. Everything here runs
//! on the machine the tests run on.

use iris_abi::Node;

use crate::batch::Batch;
use crate::decoder::Sink;
use crate::error::Result;

/// One batch, copied out.
///
/// A real host copies too, because the buffers live in the decoder's memory and the decoder is
/// entitled to reuse that memory the moment the batch has been handed over.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Recorded {
    /// How many rows the batch had.
    pub rows: u64,
    /// One entry per array, in schema pre-order.
    pub nodes: Vec<Node>,
    /// One entry per Arrow buffer, in schema pre-order.
    pub buffers: Vec<Vec<u8>>,
}

/// A sink that keeps everything it is given.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Collect {
    batches: Vec<Recorded>,
}

impl Collect {
    /// A sink with nothing in it yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            batches: Vec::new(),
        }
    }

    /// Everything the decoder emitted, in order.
    #[must_use]
    pub fn batches(&self) -> &[Recorded] {
        &self.batches
    }

    /// How many rows arrived in total.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.batches.iter().map(|b| b.rows).sum()
    }
}

impl Sink for Collect {
    fn emit(&mut self, batch: &Batch) -> Result<()> {
        // The record is built and thrown away rather than skipped, because the encoding is part of
        // what a test of a decoder should be exercising. A batch that cannot be described in a
        // record is a batch a real host would never see.
        let mut scratch = Vec::new();
        batch.record(&mut scratch)?;

        self.batches.push(Recorded {
            rows: batch.rows(),
            nodes: batch.nodes().iter().collect(),
            buffers: batch.buffers().map(<[u8]>::to_vec).collect(),
        });
        Ok(())
    }
}
