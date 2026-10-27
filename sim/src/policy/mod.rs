//! Page replacement policies.
//!
//! The buffer pool owns the frames and the page table; a policy only keeps the
//! bookkeeping it needs to pick a victim. The pool fills empty frames itself,
//! so [`Policy::victim`] is only called when every frame is occupied.
//!
//! Policies must be deterministic (no RNG): the paired A/B design relies on it.
//! To add a policy: implement [`Policy`] in a new file, add a [`PolicyKind`]
//! variant, and wire it into `name`, `FromStr` and `build`.

mod clock;
mod fifo;
mod lru;
mod opt;
mod two_q;

use std::fmt;
use std::str::FromStr;

pub use clock::Clock;
pub use fifo::Fifo;
pub use lru::Lru;
pub use opt::Opt;
pub use two_q::TwoQ;

use crate::trace::Trace;
use crate::{FrameIdx, PageId};

pub trait Policy {
    /// `page` (resident in `frame`) was referenced. `pos` is the access's
    /// index in the trace.
    fn on_hit(&mut self, frame: FrameIdx, page: PageId, pos: usize);
    /// `page` was just loaded into `frame` (which was empty or just evicted).
    fn on_load(&mut self, frame: FrameIdx, page: PageId, pos: usize);
    /// Choose a frame to evict. Only called when all frames are occupied.
    fn victim(&mut self) -> FrameIdx;
    /// Clock's reference bit, for visualisation. Other policies: false.
    fn ref_bit(&self, _frame: FrameIdx) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PolicyKind {
    Fifo,
    Lru,
    Clock,
    TwoQ,
    Opt,
}

impl PolicyKind {
    pub const ALL: [PolicyKind; 5] = [
        PolicyKind::Fifo,
        PolicyKind::Lru,
        PolicyKind::Clock,
        PolicyKind::TwoQ,
        PolicyKind::Opt,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PolicyKind::Fifo => "fifo",
            PolicyKind::Lru => "lru",
            PolicyKind::Clock => "clock",
            PolicyKind::TwoQ => "2q",
            PolicyKind::Opt => "opt",
        }
    }

    /// Instantiate for a pool of `frames` frames replaying `trace`
    /// (only OPT reads the trace, to see the future).
    pub fn build(self, frames: usize, trace: &Trace) -> Box<dyn Policy> {
        match self {
            PolicyKind::Fifo => Box::new(Fifo::new(frames)),
            PolicyKind::Lru => Box::new(Lru::new(frames)),
            PolicyKind::Clock => Box::new(Clock::new(frames)),
            PolicyKind::TwoQ => Box::new(TwoQ::new(frames)),
            PolicyKind::Opt => Box::new(Opt::new(frames, trace.next_use())),
        }
    }
}

impl fmt::Display for PolicyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for PolicyKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PolicyKind::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown policy {s:?} (expected fifo, lru, clock, 2q or opt)"))
    }
}

/// Index of the smallest value; ties go to the lowest index.
pub(crate) fn argmin(values: impl Iterator<Item = (FrameIdx, usize)>) -> Option<FrameIdx> {
    values.min_by_key(|&(i, v)| (v, i)).map(|(i, _)| i)
}
