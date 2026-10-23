//! A small Rust DSP language. Build an expression graph once, run it on the host
//! as a reference, and compile it into an installable Quad Cortex block
//! ([`device`]) built on the stock Volume framework from the user's firmware.

pub mod codegen;
pub mod device;
pub mod examples;
mod graph;
mod host;
pub mod network;

pub use graph::{
    Block, BlockBuilder, Category, Channels, Delay, MAX_NODES, MAX_PARAMETERS, MAX_STATES,
    Parameter, Signal, State,
};
pub use host::Processor;
pub use network::{Activation, Network, NetworkBuilder};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid DSP block: {0}")]
    Invalid(String),
    #[error("signal belongs to another block")]
    ForeignSignal,
    #[error("parameter {0} does not exist")]
    UnknownParameter(usize),
    #[error("parameter {name} must be finite and in [{min}, {max}]")]
    ParameterRange { name: String, min: f32, max: f32 },
    #[error("DSP value {0} is not supported by this code generator")]
    Unsupported(String),
    #[error("too few data registers: one operation needs up to three")]
    RegisterPressure,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Assembler(#[from] qc_asm::Error),
    #[error(transparent)]
    Visa(#[from] qc_asm::VisaError),
    #[error(transparent)]
    Format(#[from] qc_format::FormatError),
    #[error("stock template: {0}")]
    Template(String),
}

pub type Result<T> = std::result::Result<T, Error>;
