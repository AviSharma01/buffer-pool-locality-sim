//! Page replacement policies.
//!
//! The buffer pool owns the frames and the page table; a policy only keeps the
//! bookkeeping it needs to pick a victim. The pool fills empty frames itself,
//! so [`Policy::victim`] is only called when every frame is occupied.
//!
//! Policies must be deterministic (no RNG): the paired A/B design relies on it.
//! Each one also explains itself: it pushes a [`Note`] for every frame it
//! examines or changes (only when the [`Log`] is on), and reports per-frame
//! [`FrameState`] for the viewer.
//!
//! To add a policy: implement [`Policy`] in a new file, add a [`PolicyKind`]
//! variant, and wire it into `name`, `ALL` and `build`.

mod clock;
mod fifo;
mod innodb;
mod lru;
mod opt;
mod pg;

use std::fmt;
use std::str::FromStr;

pub use clock::Clock;
pub use fifo::Fifo;
pub use innodb::InnoDb;
pub use lru::Lru;
pub use opt::Opt;
pub use pg::Pg;

use crate::FrameIdx;
use crate::trace::Trace;

/// What a policy may know about the access being served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctx {
    /// Index of the access in the trace.
    pub pos: usize,
    /// Index of the query (tick) it belongs to.
    pub query: usize,
    /// See [`crate::trace::Access::scan`].
    pub scan: Option<u32>,
}

/// One thing a policy looked at or changed while serving an access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// A clock hand passed `frame` and lowered its count (Clock's reference
    /// bit, PostgreSQL's usage count).
    Swept { frame: FrameIdx, from: u8, to: u8 },
    /// A hit or load changed `frame`'s count.
    Counted { frame: FrameIdx, from: u8, to: u8 },
    /// The page in `frame` is evicted.
    Evict { frame: FrameIdx, why: Why },
    /// PostgreSQL: a large scan added `frame` to its ring.
    JoinedRing { frame: FrameIdx },
    /// PostgreSQL: the scan's next ring frame was too hot to recycle, so the
    /// scan swept the whole pool instead.
    RingBusy { frame: FrameIdx, usage: u8 },
    /// InnoDB: a new page entered `frame` at the midpoint (head of old).
    Inserted { frame: FrameIdx },
    /// InnoDB: touched again in a later query, so moved to the head of young.
    Promoted { frame: FrameIdx },
    /// InnoDB: touched again within the query that loaded it, so it stays old.
    StaysOld { frame: FrameIdx },
    /// InnoDB: the young/old boundary moved past `frame`.
    Relabeled { frame: FrameIdx, region: Region },
}

/// Why a frame was chosen for eviction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// FIFO: loaded before every other resident page.
    LoadedFirst { loaded_at: usize },
    /// LRU: used least recently.
    LeastRecent { last_used: usize },
    /// Clock / PostgreSQL: the hand reached it with its count at 0.
    CountZero,
    /// PostgreSQL: a scan recycles its own ring frame.
    RingReuse { usage: u8 },
    /// InnoDB: tail of the old sublist.
    OldTail,
    /// OPT: needed furthest in the future (None = never again).
    FurthestNextUse { next_use: Option<usize> },
}

/// InnoDB's two LRU sublists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Young,
    Old,
}

/// A policy's view of one frame, for overlays in the viewer. `rank` counts
/// from the next victim (0 = evicted next).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameState {
    Empty,
    Fifo { rank: usize },
    Lru { rank: usize },
    Clock { referenced: bool },
    Pg { usage: u8, in_ring: bool },
    InnoDb { region: Region, rank: usize },
    Opt { next_use: Option<usize> },
}

/// Collects [`Note`]s when on; costs one branch per push when off.
#[derive(Debug, Default)]
pub struct Log(Option<Vec<Note>>);

impl Log {
    pub fn on() -> Self {
        Self(Some(Vec::new()))
    }

    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    pub fn push(&mut self, note: Note) {
        if let Some(notes) = &mut self.0 {
            notes.push(note);
        }
    }

    /// Notes since the last take (empty when off).
    pub fn take(&mut self) -> Vec<Note> {
        self.0.as_mut().map(std::mem::take).unwrap_or_default()
    }
}

pub trait Policy {
    /// The page in `frame` was referenced.
    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log);
    /// A page was just loaded into `frame` (which was empty or just evicted).
    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log);
    /// Choose a frame to evict. Only called when all frames are occupied.
    /// Pushes a [`Note::Evict`] for the chosen frame.
    fn victim(&mut self, ctx: Ctx, log: &mut Log) -> FrameIdx;
    /// State of an occupied frame.
    fn state(&self, frame: FrameIdx) -> FrameState;
    /// Where the clock hand points, for clock-based policies.
    fn hand(&self) -> Option<FrameIdx> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PolicyKind {
    Fifo,
    Lru,
    Clock,
    Pg,
    InnoDb,
    Opt,
}

impl PolicyKind {
    pub const ALL: [PolicyKind; 6] = [
        PolicyKind::Fifo,
        PolicyKind::Lru,
        PolicyKind::Clock,
        PolicyKind::Pg,
        PolicyKind::InnoDb,
        PolicyKind::Opt,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PolicyKind::Fifo => "fifo",
            PolicyKind::Lru => "lru",
            PolicyKind::Clock => "clock",
            PolicyKind::Pg => "pg",
            PolicyKind::InnoDb => "innodb",
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
            PolicyKind::Pg => Box::new(Pg::new(frames)),
            PolicyKind::InnoDb => Box::new(InnoDb::new(frames)),
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
            .ok_or_else(|| {
                let names: Vec<_> = PolicyKind::ALL.iter().map(|k| k.name()).collect();
                format!("unknown policy {s:?} (expected {})", names.join(", "))
            })
    }
}

/// Index of the smallest value; ties go to the lowest index.
pub(crate) fn argmin(values: impl Iterator<Item = (FrameIdx, usize)>) -> Option<FrameIdx> {
    values.min_by_key(|&(i, v)| (v, i)).map(|(i, _)| i)
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::{Ctx, Log};

    /// A plain (non-scan) access at trace position `pos` in query `query`.
    pub fn at(pos: usize, query: usize) -> Ctx {
        Ctx {
            pos,
            query,
            scan: None,
        }
    }

    pub fn log() -> Log {
        Log::on()
    }
}
