//! The buffer pool and the simulation loop.

use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;

use crate::policy::{Ctx, FrameState, Log, Note, Policy, PolicyKind};
use crate::storage::{Read, StorageKind};
use crate::summary::RunSummary;
use crate::trace::{Access, Trace};
use crate::workload::Workload;
use crate::{FrameIdx, PageId};

/// Build the trace for a seeded run. Everything random about a run comes
/// from here.
pub fn build_scenario(w: &Workload, seed: u64, ticks: u64) -> Trace {
    let mut rng = StdRng::seed_from_u64(seed);
    // v0.1 shuffled the tables' order on disk before generating queries.
    // The order is gone, but drawing the same numbers keeps every seed's
    // trace identical to v0.1's.
    for i in (1..w.num_tables as usize).rev() {
        rng.gen_range(0..=i);
    }
    Trace::generate(w, &mut rng, ticks)
}

#[derive(Clone, Debug)]
struct Frame {
    page: Option<PageId>,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Hit,
    Miss { evicted: Option<Evicted> },
}

/// The page a miss pushed out of its frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Evicted {
    pub page: PageId,
    pub dirty: bool,
}

/// Fixed-size page cache. Owns frames and the page table; delegates victim
/// choice to a [`Policy`].
pub struct BufferPool {
    frames: Vec<Frame>,
    page_table: HashMap<PageId, FrameIdx>,
    next_free: usize,
    policy: Box<dyn Policy>,
}

impl BufferPool {
    pub fn new(num_frames: usize, policy: Box<dyn Policy>) -> Self {
        Self {
            frames: vec![
                Frame {
                    page: None,
                    dirty: false
                };
                num_frames
            ],
            page_table: HashMap::new(),
            next_free: 0,
            policy,
        }
    }

    /// Serve one access; returns the frame that now holds the page.
    pub fn access(&mut self, a: Access, ctx: Ctx, log: &mut Log) -> (FrameIdx, Outcome) {
        if let Some(&f) = self.page_table.get(&a.page) {
            if a.write {
                self.frames[f].dirty = true;
            }
            self.policy.on_hit(f, ctx, log);
            return (f, Outcome::Hit);
        }

        let f = if self.next_free < self.frames.len() {
            self.next_free += 1;
            self.next_free - 1
        } else {
            self.policy.victim(ctx, log)
        };

        let old = std::mem::replace(
            &mut self.frames[f],
            Frame {
                page: Some(a.page),
                dirty: a.write,
            },
        );
        let evicted = old.page.map(|page| {
            self.page_table.remove(&page);
            Evicted {
                page,
                dirty: old.dirty,
            }
        });
        self.page_table.insert(a.page, f);
        self.policy.on_load(f, ctx, log);
        (f, Outcome::Miss { evicted })
    }

    pub fn policy(&self) -> &dyn Policy {
        self.policy.as_ref()
    }

    /// Every frame: its page, and the policy's view of it.
    pub fn frame_views(&self) -> Vec<FrameView> {
        self.frames
            .iter()
            .enumerate()
            .map(|(i, f)| FrameView {
                page: f.page,
                dirty: f.dirty,
                state: match f.page {
                    Some(_) => self.policy.state(i),
                    None => FrameState::Empty,
                },
            })
            .collect()
    }

    /// Check that frames and the page table agree. For tests.
    pub fn check_consistency(&self) -> Result<(), String> {
        let mut seen = HashMap::new();
        for (i, f) in self.frames.iter().enumerate() {
            if let Some(p) = f.page {
                if let Some(j) = seen.insert(p, i) {
                    return Err(format!("page {p} in frames {j} and {i}"));
                }
                if self.page_table.get(&p) != Some(&i) {
                    return Err(format!("page table disagrees for page {p}"));
                }
            }
        }
        if seen.len() != self.page_table.len() {
            return Err("page table has entries for non-resident pages".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub frames: usize,
    pub policy: PolicyKind,
    /// Prices the run; never changes what is cached.
    pub storage: StorageKind,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            frames: 64,
            policy: PolicyKind::Clock,
            storage: StorageKind::Ssd,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub hits: u64,
    pub misses: u64,
    pub sequential_reads: u64,
    pub random_reads: u64,
    /// Misses on pages SQLite wrote without reading: no disk read.
    pub fresh_pages: u64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    pub total_ms: f64,
}

impl Metrics {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

/// One frame as an event sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FrameView {
    pub page: Option<PageId>,
    pub dirty: bool,
    pub state: FrameState,
}

/// One access, explained: what the pool did, how the page came off the
/// device and what the policy looked at. Its time follows from `read` and
/// `outcome` (a dirty eviction is a write-back) under any storage profile. Recorded only after [`Simulator::record_events`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    #[serde(flatten)]
    pub ctx: Ctx,
    pub page: PageId,
    pub write: bool,
    /// Frame now holding the page.
    pub frame: FrameIdx,
    #[serde(flatten)]
    pub outcome: Outcome,
    /// For a miss: whether the page was read sequentially or at random.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<Read>,
    /// In the order the policy did them.
    pub notes: Vec<Note>,
    /// Every frame after the access.
    pub frames: Vec<FrameView>,
    pub hand: Option<FrameIdx>,
}

pub struct Simulator {
    trace: Trace,
    cfg: SimConfig,
    pool: BufferPool,
    next_query: usize,
    /// The last page the device read or wrote. Pages sit on disk in id
    /// order, so the next one is a sequential read.
    head: Option<PageId>,
    metrics: Metrics,
    log: Log,
    events: Option<Vec<Event>>,
}

impl Simulator {
    pub fn new(trace: Trace, cfg: SimConfig) -> Self {
        assert!(cfg.frames > 0, "buffer pool needs at least one frame");
        let policy = cfg.policy.build(cfg.frames, &trace);
        Self {
            pool: BufferPool::new(cfg.frames, policy),
            trace,
            cfg,
            next_query: 0,
            head: None,
            metrics: Metrics::default(),
            log: Log::default(),
            events: None,
        }
    }

    /// Record an [`Event`] for every access from now on. Costs memory per
    /// access (all frame states), so it is meant for short captured traces.
    pub fn record_events(&mut self) {
        self.log = Log::on();
        self.events.get_or_insert_with(Vec::new);
    }

    /// Events recorded so far (empty unless recording).
    pub fn events(&self) -> &[Event] {
        self.events.as_deref().unwrap_or_default()
    }

    /// Convenience: build the scenario for `w`/`seed` and run it to the end.
    pub fn run(w: &Workload, seed: u64, ticks: u64, cfg: SimConfig) -> Self {
        let mut sim = Self::new(build_scenario(w, seed, ticks), cfg);
        while sim.step().is_some() {}
        sim
    }

    /// Ticks completed so far.
    pub fn tick(&self) -> u64 {
        self.next_query as u64
    }

    pub fn is_done(&self) -> bool {
        self.next_query >= self.trace.num_queries()
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn pool(&self) -> &BufferPool {
        &self.pool
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn policy_kind(&self) -> PolicyKind {
        self.cfg.policy
    }

    /// Run the next query and return how many have run. None when the
    /// trace is exhausted.
    pub fn step(&mut self) -> Option<u64> {
        if self.is_done() {
            return None;
        }
        let q = self.next_query;
        self.next_query += 1;
        for pos in self.trace.query_bounds(q) {
            let a = self.trace.accesses()[pos];
            let ctx = Ctx {
                pos,
                query: q,
                scan: a.scan,
            };
            self.metrics.total_ms += self.access(a, ctx);
        }
        Some(self.tick())
    }

    /// Serve one access; returns its time in milliseconds.
    fn access(&mut self, a: Access, ctx: Ctx) -> f64 {
        let (frame, outcome) = self.pool.access(a, ctx, &mut self.log);
        let profile = self.cfg.storage.profile();
        let (read, ms) = match outcome {
            Outcome::Hit => {
                self.metrics.hits += 1;
                (None, profile.hit_ms)
            }
            Outcome::Miss { evicted } => {
                self.metrics.misses += 1;
                let mut ms = 0.0;
                if let Some(e) = evicted {
                    self.metrics.evictions += 1;
                    // The modified page goes out first, so the device ends
                    // up at that page before it reads the new one.
                    if e.dirty {
                        self.metrics.dirty_evictions += 1;
                        ms += profile.write_ms;
                        self.head = Some(e.page);
                    }
                }
                if a.fresh {
                    // SQLite overwrites the whole page: the frame is filled
                    // in memory and the disk is not touched.
                    self.metrics.fresh_pages += 1;
                    return self.record(a, ctx, frame, outcome, None, ms);
                }
                let read = if self.head.is_some_and(|h| h.checked_add(1) == Some(a.page)) {
                    self.metrics.sequential_reads += 1;
                    Read::Sequential
                } else {
                    self.metrics.random_reads += 1;
                    Read::Random
                };
                self.head = Some(a.page);
                (Some(read), ms + profile.read_ms(read))
            }
        };
        self.record(a, ctx, frame, outcome, read, ms)
    }

    /// Keep the event if events are being recorded; returns `ms`.
    fn record(
        &mut self,
        a: Access,
        ctx: Ctx,
        frame: FrameIdx,
        outcome: Outcome,
        read: Option<Read>,
        ms: f64,
    ) -> f64 {
        if let Some(events) = &mut self.events {
            events.push(Event {
                ctx,
                page: a.page,
                write: a.write,
                frame,
                outcome,
                read,
                notes: self.log.take(),
                frames: self.pool.frame_views(),
                hand: self.pool.policy().hand(),
            });
        }
        ms
    }

    pub fn summary(&self) -> RunSummary {
        let m = &self.metrics;
        let queries = self.tick();
        RunSummary {
            policy: self.cfg.policy.name().to_string(),
            frames: self.cfg.frames,
            storage: self.cfg.storage,
            queries,
            accesses: m.hits + m.misses,
            hits: m.hits,
            misses: m.misses,
            hit_rate: m.hit_rate(),
            sequential_reads: m.sequential_reads,
            random_reads: m.random_reads,
            fresh_pages: m.fresh_pages,
            evictions: m.evictions,
            dirty_evictions: m.dirty_evictions,
            total_ms: m.total_ms,
            mean_ms_per_query: if queries == 0 {
                0.0
            } else {
                m.total_ms / queries as f64
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame and LRU, so every access below is a miss and the order of
    /// reads and write-backs is easy to follow.
    fn reads(pages: &[(PageId, bool)], split_after: usize) -> (Vec<Option<Read>>, Metrics) {
        let access = |&(page, write): &(PageId, bool)| Access {
            page,
            write,
            fresh: false,
            scan: None,
        };
        let queries = vec![
            pages[..split_after].iter().map(access).collect(),
            pages[split_after..].iter().map(access).collect(),
        ];
        let cfg = SimConfig {
            frames: 1,
            policy: PolicyKind::Lru,
            storage: StorageKind::Hdd,
        };
        let mut sim = Simulator::new(Trace::from_queries(1, 32, queries), cfg);
        sim.record_events();
        while sim.step().is_some() {}
        let kinds = sim.events().iter().map(|e| e.read).collect();
        (kinds, sim.metrics().clone())
    }

    #[test]
    fn reads_are_sequential_only_right_after_the_last_page_touched() {
        use Read::{Random, Sequential};
        let (kinds, m) = reads(
            &[
                (5, false),  // first read of the run: random
                (6, false),  // next page
                (8, false),  // skips one
                (9, false),  // next page, in the next query: the device stays put
                (3, true),   // random, and now modified
                (4, false),  // 3 is written back first, so 4 follows it
                (7, true),   // random
                (12, false), // 7 is written back; 12 is not next to it
            ],
            3,
        );
        let expect = [
            Random, Sequential, Random, Sequential, Random, Sequential, Random, Random,
        ];
        assert_eq!(kinds, expect.map(Some));
        assert_eq!(
            (m.sequential_reads, m.random_reads, m.dirty_evictions),
            (3, 5, 2)
        );
        let p = StorageKind::Hdd.profile();
        let ms = 3.0 * p.sequential_read_ms + 5.0 * p.random_read_ms + 2.0 * p.write_ms;
        assert!((m.total_ms - ms).abs() < 1e-9, "{} vs {ms}", m.total_ms);
    }

    /// A page SQLite writes without reading costs no read and leaves the
    /// device where it was: page 7 below is not "next" after the fresh page 6.
    #[test]
    fn a_page_written_without_reading_needs_no_read() {
        let a = |page, fresh| Access {
            page,
            write: fresh,
            fresh,
            scan: None,
        };
        let cfg = SimConfig {
            frames: 4,
            policy: PolicyKind::Lru,
            storage: StorageKind::Hdd,
        };
        let queries = vec![vec![a(5, false), a(6, true), a(7, false)]];
        let mut sim = Simulator::new(Trace::from_queries(1, 32, queries), cfg);
        sim.record_events();
        while sim.step().is_some() {}
        let reads: Vec<_> = sim.events().iter().map(|e| e.read).collect();
        assert_eq!(reads, [Some(Read::Random), None, Some(Read::Random)]);
        let m = sim.metrics();
        assert_eq!((m.misses, m.fresh_pages, m.random_reads), (3, 1, 2));
        let hdd = StorageKind::Hdd.profile();
        assert!((m.total_ms - 2.0 * hdd.random_read_ms).abs() < 1e-9);
    }

    #[test]
    fn a_hit_needs_no_read() {
        let (kinds, m) = reads(&[(5, false), (5, false)], 1);
        assert_eq!(kinds, [Some(Read::Random), None]);
        assert_eq!(m.hits, 1);
    }

    #[test]
    fn pool_stays_consistent_for_every_policy() {
        let w = Workload::builtin_default();
        for kind in PolicyKind::ALL {
            for frames in [1, 7, 64] {
                let cfg = SimConfig {
                    frames,
                    policy: kind,
                    ..SimConfig::default()
                };
                let mut sim = Simulator::new(build_scenario(&w, 3, 400), cfg);
                while sim.step().is_some() {
                    sim.pool()
                        .check_consistency()
                        .unwrap_or_else(|e| panic!("{kind} f={frames}: {e}"));
                }
            }
        }
    }
}
