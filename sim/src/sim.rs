//! The simulation loop: serves each access through the buffer pool, reads
//! ahead when the policy says so, prices the result and records events.

use std::collections::{HashMap, HashSet};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;

use crate::policy::{Ctx, Log, Note, PolicyKind};
pub use crate::pool::{BufferPool, Evicted, FrameView, Outcome};
pub use crate::readahead::ReadAhead;
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
pub struct SimConfig {
    pub frames: usize,
    pub policy: PolicyKind,
    /// Prices the run; never changes what is cached.
    pub storage: StorageKind,
    /// Off by default here, so policy tests see only the pages queries ask
    /// for; the command line defaults to auto.
    pub read_ahead: ReadAhead,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            frames: 64,
            policy: PolicyKind::Clock,
            storage: StorageKind::Ssd,
            read_ahead: ReadAhead::Off,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub hits: u64,
    pub misses: u64,
    /// Reads of the page right after the last page read.
    pub next_page_reads: u64,
    /// Reads further on, with pages skipped in between.
    pub ahead_reads: u64,
    /// Reads backwards, or the first: a full random request.
    pub jump_reads: u64,
    /// Misses on pages SQLite wrote without reading: no disk read.
    pub fresh_pages: u64,
    /// Pages loaded by read-ahead, in the same request as a missed page.
    pub pages_read_ahead: u64,
    /// Hits on a page read ahead, at its first use.
    pub read_ahead_hits: u64,
    /// Pages read ahead and evicted before anyone used them.
    pub read_ahead_unused: u64,
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

/// What serving one access did, for its event.
#[derive(Default)]
struct Done {
    ms: f64,
    read: Option<Read>,
    ahead: Vec<PageId>,
    ahead_evicted: Vec<Evicted>,
    read_ahead_hit: bool,
}

/// One access, explained: what the pool did, how the page came off the
/// device and what the policy looked at. Its time follows from `read`,
/// `ahead` and the evictions (a dirty one is a write-back) under any storage
/// profile. Recorded only after [`Simulator::record_events`].
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
    /// For a miss that read the page: where it was relative to the last page
    /// read (`read`: next, ahead with `skipped`, or jump). None for a hit or
    /// a page SQLite wrote without reading.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub read: Option<Read>,
    /// Pages read ahead in the same request as this miss, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ahead: Vec<PageId>,
    /// Pages those read-ahead loads evicted (written back first if dirty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ahead_evicted: Vec<Evicted>,
    /// A hit on a page that was read ahead and not used until now.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub read_ahead_hit: bool,
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
    /// The last page read from the device: where the read stream is. Pages
    /// sit on disk in id order.
    last_read: Option<PageId>,
    /// Last page accessed in each table or index, to spot streams.
    streams: HashMap<u32, PageId>,
    /// Pages read ahead that nobody has used yet.
    waiting: HashSet<PageId>,
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
            last_read: None,
            streams: HashMap::new(),
            waiting: HashSet::new(),
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
        // Each table or index is its own stream: does this access follow the
        // previous access to the same object?
        let owner = self.trace.owner_of(a.page);
        let continues = owner
            .and_then(|o| self.streams.insert(o, a.page))
            .is_some_and(|last| last.checked_add(1) == Some(a.page));
        let (frame, outcome) = self.pool.access(a, ctx, &mut self.log);
        let profile = self.cfg.storage.profile();
        let mut done = Done::default();
        match outcome {
            Outcome::Hit => {
                self.metrics.hits += 1;
                done.ms = profile.hit_ms;
                done.read_ahead_hit = self.waiting.remove(&a.page);
                self.metrics.read_ahead_hits += u64::from(done.read_ahead_hit);
            }
            Outcome::Miss { evicted } => {
                self.metrics.misses += 1;
                self.evicted(evicted, &mut done.ms);
                if a.fresh {
                    // SQLite overwrites the whole page: the frame is filled
                    // in memory and the disk is not touched.
                    self.metrics.fresh_pages += 1;
                } else {
                    self.read(a, ctx, owner, continues, &mut done);
                }
            }
        }
        self.record(a, ctx, frame, outcome, done)
    }

    /// Count an eviction and price its write-back: a separate write request
    /// that leaves the read stream where it was.
    fn evicted(&mut self, evicted: Option<Evicted>, ms: &mut f64) {
        if let Some(e) = evicted {
            self.metrics.evictions += 1;
            if e.dirty {
                self.metrics.dirty_evictions += 1;
                *ms += self.cfg.storage.profile().write_ms();
            }
            if self.waiting.remove(&e.page) {
                self.metrics.read_ahead_unused += 1;
            }
        }
    }

    /// Read a missed page, and the pages after it if the policy reads
    /// ahead, in one request.
    fn read(&mut self, a: Access, ctx: Ctx, owner: Option<u32>, continues: bool, done: &mut Done) {
        let read = Read::of(a.page, self.last_read);
        match read {
            Read::Next => self.metrics.next_page_reads += 1,
            Read::Ahead { .. } => self.metrics.ahead_reads += 1,
            Read::Jump => self.metrics.jump_reads += 1,
        }
        let policy = self.pool.policy();
        let cap = policy.read_ahead_cap(ctx, self.cfg.read_ahead.cap(self.cfg.frames));
        if cap > 0 && owner.is_some() && policy.reads_ahead(ctx, continues) {
            // Same table or index only, stopping at another object's page, a
            // free page, the end of the file or a page already in the pool.
            let mut page = a.page + 1;
            while done.ahead.len() < cap
                && self.trace.owner_of(page) == owner
                && !self.pool.contains(page)
            {
                let evicted = self.pool.read_ahead(page, a.page, ctx, &mut self.log);
                self.evicted(evicted, &mut done.ms);
                done.ahead_evicted.extend(evicted);
                self.waiting.insert(page);
                done.ahead.push(page);
                page += 1;
            }
            self.pool.request_done(a.page);
        }
        let pages = done.ahead.len() as u32;
        self.metrics.pages_read_ahead += u64::from(pages);
        self.last_read = Some(a.page + pages);
        done.read = Some(read);
        done.ms += self.cfg.storage.profile().read_ms(read, 1 + pages);
    }

    /// Keep the event if events are being recorded; returns its time.
    fn record(
        &mut self,
        a: Access,
        ctx: Ctx,
        frame: FrameIdx,
        outcome: Outcome,
        done: Done,
    ) -> f64 {
        if let Some(events) = &mut self.events {
            events.push(Event {
                ctx,
                page: a.page,
                write: a.write,
                frame,
                outcome,
                read: done.read,
                ahead: done.ahead,
                ahead_evicted: done.ahead_evicted,
                read_ahead_hit: done.read_ahead_hit,
                notes: self.log.take(),
                frames: self.pool.frame_views(),
                hand: self.pool.policy().hand(),
            });
        }
        done.ms
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
            next_page_reads: m.next_page_reads,
            ahead_reads: m.ahead_reads,
            jump_reads: m.jump_reads,
            fresh_pages: m.fresh_pages,
            read_requests: m.next_page_reads + m.ahead_reads + m.jump_reads,
            read_ahead_cap: if self.cfg.policy == PolicyKind::Opt {
                0
            } else {
                self.cfg.read_ahead.cap(self.cfg.frames) as u32
            },
            pages_read_ahead: m.pages_read_ahead,
            read_ahead_hits: m.read_ahead_hits,
            read_ahead_unused: m.read_ahead_unused,
            dirty_at_end: self.pool.frame_views().iter().filter(|f| f.dirty).count() as u64,
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
            ..SimConfig::default()
        };
        let mut sim = Simulator::new(Trace::from_queries(1, 32, queries), cfg);
        sim.record_events();
        while sim.step().is_some() {}
        let kinds = sim.events().iter().map(|e| e.read).collect();
        (kinds, sim.metrics().clone())
    }

    #[test]
    fn reads_are_placed_relative_to_the_last_page_read() {
        use Read::{Ahead, Jump, Next};
        let (kinds, m) = reads(
            &[
                (5, false),  // first read of the run: a jump
                (6, false),  // next page
                (8, false),  // one page further on
                (9, false),  // next page, in the next query: the stream carries on
                (3, true),   // backwards: a jump, and now modified
                (4, false),  // 3 is written back first, but 4 still follows 3's read
                (7, true),   // further on
                (12, false), // 7 is written back; 12 is 4 pages on from 7's read
            ],
            3,
        );
        let expect = [
            Jump,
            Next,
            Ahead { skipped: 1 },
            Next,
            Jump,
            Next,
            Ahead { skipped: 2 },
            Ahead { skipped: 4 },
        ];
        assert_eq!(kinds, expect.map(Some));
        assert_eq!(
            (
                m.next_page_reads,
                m.ahead_reads,
                m.jump_reads,
                m.dirty_evictions
            ),
            (3, 3, 2, 2)
        );
        let p = StorageKind::Hdd.profile();
        let ms: f64 = expect.iter().map(|&r| p.read_ms(r, 1)).sum::<f64>() + 2.0 * p.write_ms();
        assert!((m.total_ms - ms).abs() < 1e-9, "{} vs {ms}", m.total_ms);
    }

    /// A page SQLite writes without reading costs no read and leaves the
    /// read stream where it was: page 7 below is one page on from 5, not
    /// "next" after the fresh page 6.
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
            ..SimConfig::default()
        };
        let queries = vec![vec![a(5, false), a(6, true), a(7, false)]];
        let mut sim = Simulator::new(Trace::from_queries(1, 32, queries), cfg);
        sim.record_events();
        while sim.step().is_some() {}
        let reads: Vec<_> = sim.events().iter().map(|e| e.read).collect();
        assert_eq!(
            reads,
            [Some(Read::Jump), None, Some(Read::Ahead { skipped: 1 })]
        );
        let m = sim.metrics();
        assert_eq!(
            (m.misses, m.fresh_pages, m.jump_reads, m.ahead_reads),
            (3, 1, 1, 1)
        );
        let hdd = StorageKind::Hdd.profile();
        let ms = hdd.read_ms(Read::Jump, 1) + hdd.read_ms(Read::Ahead { skipped: 1 }, 1);
        assert!((m.total_ms - ms).abs() < 1e-9);
    }

    /// Writing a modified page back does not move the read stream: page 21
    /// follows page 20's read even though page 10 is written out in between.
    #[test]
    fn a_write_back_leaves_the_read_stream_alone() {
        let a = |page, write| Access {
            page,
            write,
            fresh: false,
            scan: None,
        };
        let cfg = SimConfig {
            frames: 2,
            policy: PolicyKind::Lru,
            storage: StorageKind::Hdd,
            ..SimConfig::default()
        };
        let queries = vec![vec![a(10, true), a(20, false), a(21, false)]];
        let mut sim = Simulator::new(Trace::from_queries(1, 32, queries), cfg);
        sim.record_events();
        while sim.step().is_some() {}
        let reads: Vec<_> = sim.events().iter().map(|e| e.read).collect();
        assert_eq!(
            reads,
            [
                Some(Read::Jump),
                Some(Read::Ahead { skipped: 9 }),
                Some(Read::Next)
            ]
        );
        assert_eq!(sim.metrics().dirty_evictions, 1);
    }

    /// Two tables: pages 1-10 and 11-20.
    fn two_tables(queries: Vec<Vec<PageId>>) -> Trace {
        let owners = (0..=20)
            .map(|p| (p >= 1).then(|| u32::from(p > 10)))
            .collect();
        let queries = queries
            .into_iter()
            .map(|q| {
                q.into_iter()
                    .map(|page| Access {
                        page,
                        write: false,
                        fresh: false,
                        scan: None,
                    })
                    .collect()
            })
            .collect();
        Trace::from_queries(1, 21, queries).with_owners(owners)
    }

    fn replay_ahead(trace: Trace, read_ahead: ReadAhead) -> Simulator {
        let cfg = SimConfig {
            frames: 16,
            policy: PolicyKind::Lru,
            storage: StorageKind::Ssd,
            read_ahead,
        };
        let mut sim = Simulator::new(trace, cfg);
        sim.record_events();
        while sim.step().is_some() {}
        sim
    }

    /// The second page in a row of a table triggers read-ahead of up to a
    /// quarter of the pool (4 pages here), all in the missed page's request;
    /// it never crosses into the next table.
    #[test]
    fn read_ahead_follows_a_table_and_stops_at_its_edge() {
        let sim = replay_ahead(two_tables(vec![(3..=12).collect()]), ReadAhead::Auto);
        let ahead: Vec<_> = sim.events().iter().map(|e| e.ahead.clone()).collect();
        let none = Vec::new;
        // 3 starts the stream; 4 continues it and reads 5-8 ahead; 5-8 are
        // hits; 9 continues and reads 10, the table's last page; 11 starts
        // the next table, 12 continues it.
        assert_eq!(
            ahead,
            [
                none(),
                vec![5, 6, 7, 8],
                none(),
                none(),
                none(),
                none(),
                vec![10],
                none(),
                none(),
                vec![13, 14, 15, 16]
            ]
        );
        let hits: Vec<_> = sim.events().iter().map(|e| e.read_ahead_hit).collect();
        assert_eq!(
            hits,
            [
                false, false, true, true, true, true, false, true, false, false
            ]
        );
        let m = sim.metrics();
        assert_eq!(
            (m.pages_read_ahead, m.read_ahead_hits, m.read_ahead_unused),
            (9, 5, 0)
        );
        // Five requests: 3; 4 with 5-8; 9 with 10; 11; 12 with 13-16.
        let s = sim.summary();
        assert_eq!(s.read_requests, 5);
        let p = StorageKind::Ssd.profile();
        let ms = p.read_ms(Read::Jump, 1)
            + p.read_ms(Read::Next, 5)
            + 4.0 * p.hit_ms
            + p.read_ms(Read::Next, 2)
            + p.hit_ms
            + p.read_ms(Read::Next, 1)
            + p.read_ms(Read::Next, 5);
        assert!((m.total_ms - ms).abs() < 1e-12, "{} vs {ms}", m.total_ms);
    }

    #[test]
    fn read_ahead_stops_at_a_page_already_in_the_pool() {
        let sim = replay_ahead(two_tables(vec![vec![6], vec![3, 4]]), ReadAhead::Auto);
        assert_eq!(sim.events()[2].ahead, [5]);
    }

    #[test]
    fn off_or_lookups_read_nothing_ahead() {
        let sim = replay_ahead(two_tables(vec![(3..=12).collect()]), ReadAhead::Off);
        assert_eq!(sim.metrics().pages_read_ahead, 0);
        // Lookups that jump around never continue a table.
        let sim = replay_ahead(two_tables(vec![vec![3, 7, 2, 9, 4]]), ReadAhead::Auto);
        assert_eq!(sim.metrics().pages_read_ahead, 0);
    }

    #[test]
    fn a_hit_needs_no_read() {
        let (kinds, m) = reads(&[(5, false), (5, false)], 1);
        assert_eq!(kinds, [Some(Read::Jump), None]);
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
