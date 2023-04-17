#![warn(missing_docs, unreachable_pub)]
#![deny(unused_must_use, rust_2018_idioms, unused_crate_dependencies)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]

//! The implementation of Engine API.
//! [Read more](https://github.com/ethereum/execution-apis/tree/main/src/engine).

/// The Engine API implementation.
mod engine_api;

/// The Engine API message type.
mod message;

/// Engine API error.
mod error;

pub use engine_api::{EngineApi, EngineApiSender};
pub use error::*;
pub use message::EngineApiMessageVersion;

// re-export server trait for convenience
pub use reth_rpc_api::EngineApiServer;
