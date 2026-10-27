//! Buffer pool + disk layout simulator.
//!
//! Pipeline: a [`Workload`] (TOML) plus a seed produces a [`trace::Trace`] up
//! front. A [`Simulator`] replays that trace through a buffer pool with a
//! pluggable replacement [`policy::Policy`], optionally reclustering tables on
//! disk as it goes.
//!
//! All randomness lives in trace generation. Policies and reclustering are
//! deterministic, so two runs with the same seed see the identical access
//! stream and differ only in the knob being tested (paired A/B comparison).
//!
//! This crate does no file I/O so it can be compiled to WebAssembly.

pub mod layout;
pub mod policy;
pub mod sim;
pub mod snapshot;
pub mod trace;
pub mod workload;

pub type PageId = u32;
pub type TableId = u16;
pub type FrameIdx = usize;

pub use policy::PolicyKind;
pub use sim::{SimConfig, Simulator, build_scenario};
pub use workload::{CostModel, Workload};
