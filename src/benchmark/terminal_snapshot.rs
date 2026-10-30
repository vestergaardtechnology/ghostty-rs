//! Port of `src/benchmark/TerminalSnapshot.zig`.
//!
//! STATUS: not yet ported.

use std::convert::Infallible;

use super::benchmark::{Benchmark, VTable};
use super::cli::BenchmarkImpl;
use crate::cli::args::cli_struct;

pub struct TerminalSnapshot {
    pub opts: Options,
}

cli_struct! {
    #[derive(Debug, Clone)]
    pub struct Options {
        pub data: Option<String> = None,
    }
}

impl BenchmarkImpl for TerminalSnapshot {
    type Options = Options;
    type CreateError = Infallible;

    fn create(opts: Options) -> Result<Box<TerminalSnapshot>, Infallible> {
        Ok(Box::new(TerminalSnapshot { opts }))
    }

    fn benchmark(&mut self) -> Benchmark<'_> {
        Benchmark::init(self, VTable::new(|_| Ok(())))
    }
}
