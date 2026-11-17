//! The buffer pool and the simulation loop.

use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;

use crate::policy::{Ctx, FrameState, Log, Note, Policy, PolicyKind};
use crate::summary::RunSummary;
use crate::trace::{Access, Trace};
use crate::workload::{CostModel, Workload};
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
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            frames: 64,
            policy: PolicyKind::Clock,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    pub total_cost: f64,
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

/// What one access cost, kept in parts so storage detail can be added to
/// each later (units from the [`CostModel`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Cost {
    /// A hit reads the frame; a miss reads the page from disk.
    pub access: f64,
    /// A miss moves the disk head from the previous read.
    pub seek: f64,
    /// A miss that evicts a dirty page writes it back first.
    pub writeback: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.access + self.seek + self.writeback
    }
}

/// One access, explained: what the pool did, what it cost and what the
/// policy looked at. Recorded only after [`Simulator::record_events`].
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
    pub cost: Cost,
    /// In the order the policy did them.
    pub notes: Vec<Note>,
    /// Every frame after the access.
    pub frames: Vec<FrameView>,
    pub hand: Option<FrameIdx>,
}

pub struct Simulator {
    trace: Trace,
    cost: CostModel,
    cfg: SimConfig,
    pool: BufferPool,
    next_query: usize,
    last_disk_pos: u64,
    metrics: Metrics,
    log: Log,
    events: Option<Vec<Event>>,
}

impl Simulator {
    pub fn new(trace: Trace, cost: CostModel, cfg: SimConfig) -> Self {
        assert!(cfg.frames > 0, "buffer pool needs at least one frame");
        let policy = cfg.policy.build(cfg.frames, &trace);
        Self {
            pool: BufferPool::new(cfg.frames, policy),
            trace,
            cost,
            cfg,
            next_query: 0,
            last_disk_pos: 0,
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
        let mut sim = Self::new(build_scenario(w, seed, ticks), w.cost.clone(), cfg);
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

    pub fn cost_model(&self) -> &CostModel {
        &self.cost
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
            self.metrics.total_cost += self.access(a, ctx);
        }
        Some(self.tick())
    }

    /// Serve one access; returns its cost.
    fn access(&mut self, a: Access, ctx: Ctx) -> f64 {
        let (frame, outcome) = self.pool.access(a, ctx, &mut self.log);
        let cost = match outcome {
            Outcome::Hit => {
                self.metrics.hits += 1;
                Cost {
                    access: self.cost.hit,
                    ..Cost::default()
                }
            }
            Outcome::Miss { evicted } => {
                self.metrics.misses += 1;
                // Pages sit on disk in id order.
                let disk_pos = a.page as u64;
                let seek_dist = disk_pos.abs_diff(self.last_disk_pos);
                let total_disk = self.trace.num_tables as u64 * self.trace.pages_per_table as u64;
                let seek = (seek_dist as f64 / total_disk as f64) * self.cost.seek_max;
                self.last_disk_pos = disk_pos;

                let mut writeback = 0.0;
                if let Some(e) = evicted {
                    self.metrics.evictions += 1;
                    if e.dirty {
                        self.metrics.dirty_evictions += 1;
                        writeback = self.cost.dirty_writeback;
                    }
                }
                Cost {
                    access: self.cost.miss,
                    seek,
                    writeback,
                }
            }
        };
        if let Some(events) = &mut self.events {
            events.push(Event {
                ctx,
                page: a.page,
                write: a.write,
                frame,
                outcome,
                cost,
                notes: self.log.take(),
                frames: self.pool.frame_views(),
                hand: self.pool.policy().hand(),
            });
        }
        cost.total()
    }

    pub fn summary(&self) -> RunSummary {
        let m = &self.metrics;
        let queries = self.tick();
        RunSummary {
            policy: self.cfg.policy.name().to_string(),
            frames: self.cfg.frames,
            queries,
            accesses: m.hits + m.misses,
            hits: m.hits,
            misses: m.misses,
            hit_rate: m.hit_rate(),
            evictions: m.evictions,
            dirty_evictions: m.dirty_evictions,
            total_cost: m.total_cost,
            mean_cost_per_query: if queries == 0 {
                0.0
            } else {
                m.total_cost / queries as f64
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_stays_consistent_for_every_policy() {
        let w = Workload::builtin_default();
        for kind in PolicyKind::ALL {
            for frames in [1, 7, 64] {
                let cfg = SimConfig {
                    frames,
                    policy: kind,
                };
                let mut sim = Simulator::new(build_scenario(&w, 3, 400), w.cost.clone(), cfg);
                while sim.step().is_some() {
                    sim.pool()
                        .check_consistency()
                        .unwrap_or_else(|e| panic!("{kind} f={frames}: {e}"));
                }
            }
        }
    }
}
