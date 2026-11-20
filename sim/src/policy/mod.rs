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

use serde::Serialize;

pub use clock::Clock;
pub use fifo::Fifo;
pub use innodb::InnoDb;
pub use lru::Lru;
pub use opt::Opt;
pub use pg::Pg;

use crate::FrameIdx;
use crate::trace::Trace;

/// What a policy may know about the access being served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Ctx {
    /// Index of the access in the trace.
    pub pos: usize,
    /// Index of the query (tick) it belongs to.
    pub query: usize,
    /// See [`crate::trace::Access::scan`].
    pub scan: Option<u32>,
}

/// One thing a policy looked at or changed while serving an access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    /// InnoDB: first use of a page that was read ahead. Its clock starts now,
    /// so it stays old; a use in a later query would promote it.
    FirstUse { frame: FrameIdx },
    /// InnoDB: the young/old boundary moved past `frame`.
    Relabeled { frame: FrameIdx, region: Region },
}

/// Why a frame was chosen for eviction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    Young,
    Old,
}

/// A policy's view of one frame, for overlays in the viewer. `rank` counts
/// from the next victim (0 = evicted next).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameState {
    Empty,
    Fifo {
        rank: usize,
    },
    Lru {
        rank: usize,
    },
    Clock {
        referenced: bool,
    },
    Pg {
        usage: u8,
        in_ring: bool,
    },
    #[serde(rename = "innodb")]
    InnoDb {
        region: Region,
        rank: usize,
    },
    Opt {
        next_use: Option<usize>,
    },
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
    /// A page nobody asked for yet was read ahead into `frame`. By default it
    /// enters like any loaded page.
    fn on_read_ahead(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        self.on_load(frame, ctx, log);
    }
    /// Whether a miss should read ahead. `continues` is true when the missed
    /// page follows the previous access to the same table or index (a stream,
    /// as Linux and InnoDB detect one). By default that is the trigger.
    fn reads_ahead(&self, _ctx: Ctx, continues: bool) -> bool {
        continues
    }
    /// The request that loaded `requested` (and any pages read ahead with it)
    /// is done; the page asked for is used now.
    fn on_request_done(&mut self, _requested: FrameIdx) {}
    /// How many pages to read ahead at most, given the pool-wide cap.
    fn read_ahead_cap(&self, _ctx: Ctx, cap: usize) -> usize {
        cap
    }
    /// Choose a frame to evict. Only called when all frames are occupied.
    /// Pushes a [`Note::Evict`] for the chosen frame.
    /// Never returns `pinned`: the frame of the page whose request is being
    /// served, which read-ahead must not evict (PostgreSQL pins it too).
    fn victim(&mut self, ctx: Ctx, pinned: Option<FrameIdx>, log: &mut Log) -> FrameIdx;
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

#[cfg(test)]
mod pin_tests {
    use super::test_util::{at, log};
    use super::*;

    /// A fresh policy of `kind` over 8 full frames, with a few hits.
    fn primed(kind: PolicyKind, scan: Option<u32>) -> Box<dyn Policy> {
        let mut p: Box<dyn Policy> = match kind {
            PolicyKind::Fifo => Box::new(Fifo::new(8)),
            PolicyKind::Lru => Box::new(Lru::new(8)),
            PolicyKind::Clock => Box::new(Clock::new(8)),
            PolicyKind::Pg => Box::new(Pg::new(8)),
            PolicyKind::InnoDb => Box::new(InnoDb::new(8)),
            PolicyKind::Opt => Box::new(Opt::new(8, (0..20).map(|i| 30 - i).collect())),
        };
        let mut l = log();
        for f in 0..8 {
            p.on_load(
                f,
                Ctx {
                    pos: f,
                    query: 0,
                    scan,
                },
                &mut l,
            );
        }
        // Same query as the eviction, so a scan's ring is still in force.
        p.on_hit(5, at(8, 0), &mut l);
        p.on_hit(2, at(9, 0), &mut l);
        p
    }

    /// Whatever frame a policy would evict, pinning it makes it pick another.
    #[test]
    fn no_policy_evicts_the_pinned_frame() {
        for kind in PolicyKind::ALL {
            for scan in [None, Some(100)] {
                let ctx = Ctx {
                    pos: 10,
                    query: 0,
                    scan,
                };
                let first = primed(kind, scan).victim(ctx, None, &mut log());
                let other = primed(kind, scan).victim(ctx, Some(first), &mut log());
                assert_ne!(
                    first, other,
                    "{kind} (scan {scan:?}) evicted its pinned frame"
                );
            }
        }
    }
}
