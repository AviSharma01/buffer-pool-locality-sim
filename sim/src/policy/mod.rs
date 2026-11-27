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
//! variant, and wire it into `name`, `ALL` and `build` (and `id` and `set` if
//! it has settings).

mod clock;
mod fifo;
mod innodb;
mod list;
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

/// A policy and its settings. Only PostgreSQL-style and InnoDB-style have
/// settings; each defaults to the policy as described in its own file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PolicyKind {
    Fifo,
    Lru,
    Clock,
    Pg(PgSettings),
    InnoDb(InnoDbSettings),
    Opt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PgSettings {
    /// Whether big scans recycle a ring of frames (on by default). Off is not
    /// a PostgreSQL setting; it exists to measure what the ring does.
    pub ring: bool,
}

impl PgSettings {
    pub const DEFAULT: Self = Self { ring: true };
}

impl Default for PgSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InnoDbSettings {
    /// The old sublist as a percentage of the list, like MySQL's
    /// innodb_old_blocks_pct (5 to 95). None is the default, exactly 3/8.
    pub old_pct: Option<u8>,
    /// What moves an old page to the head of young.
    pub promote: Promote,
}

impl InnoDbSettings {
    pub const DEFAULT: Self = Self {
        old_pct: None,
        promote: Promote::LaterQuery,
    };
    pub const OLD_PCT: std::ops::RangeInclusive<u8> = 5..=95;
}

impl Default for InnoDbSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Promote {
    /// A use in a later query than the page's first use (the default).
    LaterQuery,
    /// Any use after the first, in the same query or not; stands in for
    /// innodb_old_blocks_time = 0.
    AnyReuse,
}

impl PolicyKind {
    pub const ALL: [PolicyKind; 6] = [
        PolicyKind::Fifo,
        PolicyKind::Lru,
        PolicyKind::Clock,
        PolicyKind::Pg(PgSettings::DEFAULT),
        PolicyKind::InnoDb(InnoDbSettings::DEFAULT),
        PolicyKind::Opt,
    ];

    /// The policy's name, without its settings.
    pub fn name(self) -> &'static str {
        match self {
            PolicyKind::Fifo => "fifo",
            PolicyKind::Lru => "lru",
            PolicyKind::Clock => "clock",
            PolicyKind::Pg(_) => "pg",
            PolicyKind::InnoDb(_) => "innodb",
            PolicyKind::Opt => "opt",
        }
    }

    /// The name with every setting that differs from the default, in a fixed
    /// order: `innodb:old=20:promote=any`, `pg:ring=off`, or just `lru`. It
    /// names the policy in run and sweep files and parses back to the same
    /// policy. Only `[a-z0-9:=]`, so it is safe in a file name once `:` is
    /// replaced (see [`PolicyKind::file_name`]).
    pub fn id(self) -> String {
        let mut id = self.name().to_string();
        match self {
            PolicyKind::Pg(s) if !s.ring => id.push_str(":ring=off"),
            PolicyKind::InnoDb(s) => {
                if let Some(pct) = s.old_pct {
                    id.push_str(&format!(":old={pct}"));
                }
                if s.promote == Promote::AnyReuse {
                    id.push_str(":promote=any");
                }
            }
            _ => {}
        }
        id
    }

    /// The id as a folder name: `innodb-old-20`.
    pub fn file_name(self) -> String {
        self.id().replace([':', '='], "-")
    }

    /// Instantiate for a pool of `frames` frames replaying `trace`
    /// (only OPT reads the trace, to see the future).
    pub fn build(self, frames: usize, trace: &Trace) -> Box<dyn Policy> {
        match self {
            PolicyKind::Fifo => Box::new(Fifo::new(frames)),
            PolicyKind::Lru => Box::new(Lru::new(frames)),
            PolicyKind::Clock => Box::new(Clock::new(frames)),
            PolicyKind::Pg(s) => Box::new(Pg::with(frames, s)),
            PolicyKind::InnoDb(s) => Box::new(InnoDb::with(frames, s)),
            PolicyKind::Opt => Box::new(Opt::new(frames, trace.next_use())),
        }
    }
}

impl fmt::Display for PolicyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id())
    }
}

/// Parses `name` or `name:key=value:key=value`, as [`PolicyKind::id`] writes
/// it (any order, any case for the name; each key at most once).
impl FromStr for PolicyKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(':');
        let name = parts.next().unwrap_or_default();
        let mut kind = PolicyKind::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                let names: Vec<_> = PolicyKind::ALL.iter().map(|k| k.name()).collect();
                format!("unknown policy {name:?} (expected {})", names.join(", "))
            })?;
        let mut seen = Vec::new();
        for setting in parts {
            let (key, value) = setting
                .split_once('=')
                .ok_or_else(|| format!("policy setting {setting:?} must be key=value"))?;
            if seen.contains(&key) {
                return Err(format!("policy setting {key:?} is given twice in {s:?}"));
            }
            seen.push(key);
            kind = kind.set(key, value)?;
        }
        Ok(kind)
    }
}

impl PolicyKind {
    fn set(self, key: &str, value: &str) -> Result<Self, String> {
        match (self, key) {
            (PolicyKind::Pg(mut s), "ring") => {
                s.ring = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(format!("pg ring must be on or off, got {value:?}")),
                };
                Ok(PolicyKind::Pg(s))
            }
            (PolicyKind::InnoDb(mut s), "old") => {
                let range = InnoDbSettings::OLD_PCT;
                let pct = value
                    .parse::<u8>()
                    .ok()
                    .filter(|p| range.contains(p))
                    .ok_or_else(|| {
                        format!(
                            "innodb old must be a whole percentage from {} to {}, got {value:?}",
                            range.start(),
                            range.end()
                        )
                    })?;
                s.old_pct = Some(pct);
                Ok(PolicyKind::InnoDb(s))
            }
            (PolicyKind::InnoDb(mut s), "promote") => {
                s.promote = match value {
                    "later" => Promote::LaterQuery,
                    "any" => Promote::AnyReuse,
                    _ => {
                        return Err(format!(
                            "innodb promote must be later or any, got {value:?}"
                        ));
                    }
                };
                Ok(PolicyKind::InnoDb(s))
            }
            (PolicyKind::Pg(_), _) => Err(format!("unknown pg setting {key:?} (expected ring)")),
            (PolicyKind::InnoDb(_), _) => Err(format!(
                "unknown innodb setting {key:?} (expected old, promote)"
            )),
            _ => Err(format!("{} has no settings, got {key:?}", self.name())),
        }
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
mod kind_tests {
    use super::*;

    #[test]
    fn ids_parse_back_to_the_same_policy() {
        for id in [
            "fifo",
            "lru",
            "clock",
            "pg",
            "pg:ring=off",
            "innodb",
            "innodb:old=5",
            "innodb:old=95",
            "innodb:promote=any",
            "innodb:old=20:promote=any",
            "opt",
        ] {
            let kind: PolicyKind = id.parse().unwrap();
            assert_eq!(kind.id(), id);
        }
        let kind: PolicyKind = "InnoDB:promote=any:old=20".parse().unwrap();
        assert_eq!(
            kind.id(),
            "innodb:old=20:promote=any",
            "settings in a fixed order"
        );
        let kind: PolicyKind = "pg:ring=on".parse().unwrap();
        assert_eq!(
            kind,
            PolicyKind::ALL[3],
            "a default setting is the plain policy"
        );
        assert_eq!(kind.id(), "pg");
        assert_eq!(
            "innodb:old=20:promote=any"
                .parse::<PolicyKind>()
                .unwrap()
                .file_name(),
            "innodb-old-20-promote-any"
        );
    }

    #[test]
    fn bad_settings_are_refused_with_what_is_allowed() {
        for (spec, says) in [
            ("lru:old=20", "lru has no settings"),
            ("pg:old=20", "expected ring"),
            ("pg:ring=maybe", "on or off"),
            ("innodb:old=4", "from 5 to 95"),
            ("innodb:old=96", "from 5 to 95"),
            ("innodb:old=20.5", "whole percentage"),
            ("innodb:old=-1", "whole percentage"),
            ("innodb:old=300", "whole percentage"),
            ("innodb:old=", "whole percentage"),
            ("innodb:promote=sometimes", "later or any"),
            ("innodb:size=2", "expected old, promote"),
            ("innodb:old", "key=value"),
            ("innodb:", "key=value"),
            ("innodb:old=20:old=30", "given twice"),
            ("innodb/old=20", "unknown policy"),
            ("", "unknown policy"),
        ] {
            let err = spec.parse::<PolicyKind>().unwrap_err();
            assert!(err.contains(says), "{spec:?}: {err}");
        }
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
            PolicyKind::Pg(s) => Box::new(Pg::with(8, s)),
            PolicyKind::InnoDb(s) => Box::new(InnoDb::with(8, s)),
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
        let settings = [
            "pg:ring=off",
            "innodb:old=5",
            "innodb:old=95",
            "innodb:promote=any",
        ];
        let kinds = PolicyKind::ALL
            .into_iter()
            .chain(settings.map(|s| s.parse().unwrap()));
        for kind in kinds {
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
