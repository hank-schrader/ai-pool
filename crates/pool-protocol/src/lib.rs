//! Types shared by the ai-pool server and miner.

pub mod catalog;
pub mod messages;
pub mod requests;

pub use catalog::{Accelerator, Backend, Capability, Catalog, Model, Profile};
pub use messages::{MinerMessage, Operation, PoolMessage};
