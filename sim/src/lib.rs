//! Buffer pool simulator.
//!
//! A trace of page accesses comes either from a page trace captured from
//! SQLite ([`CapturedTrace`]) or from a synthetic [`Workload`] (TOML) plus a
//! seed. A [`Simulator`] replays it through a buffer pool with a pluggable
//! replacement [`policy::Policy`].
//!
//! All randomness lives in trace generation and policies are deterministic,
//! so every policy sees the identical access stream (paired comparison).
//!
//! This crate does no file I/O so it can be compiled to WebAssembly.

pub mod capture;
pub mod policy;
pub mod run;
pub mod sim;
pub mod storage;
pub mod summary;
pub mod trace;
pub mod workload;

pub type PageId = u32;
pub type TableId = u16;
pub type FrameIdx = usize;

pub use capture::CapturedTrace;
pub use policy::PolicyKind;
pub use sim::{SimConfig, Simulator, build_scenario};
pub use storage::StorageKind;
pub use workload::Workload;
