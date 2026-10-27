//! The buffer pool and the simulation loop.

use std::collections::{BTreeSet, HashMap};

use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::layout::{self, CoAccess, Layout};
use crate::policy::{Policy, PolicyKind};
use crate::snapshot::{FrameSnap, MetricsSnap, RunSummary, Snapshot};
use crate::trace::{Access, Trace};
use crate::workload::{CostModel, Workload};
use crate::{FrameIdx, PageId, TableId};

/// Smoothing factor for the cost and locality moving averages.
const EMA_ALPHA: f64 = 0.02;

/// Build the trace and initial (scrambled) disk layout for a seeded run.
/// Everything random about a run comes from here.
pub fn build_scenario(w: &Workload, seed: u64, ticks: u64) -> (Trace, Vec<TableId>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let initial_layout = layout::shuffled(w.num_tables, &mut rng);
    let trace = Trace::generate(w, &mut rng, ticks);
    (trace, initial_layout)
}

#[derive(Clone, Debug)]
struct Frame {
    page: Option<PageId>,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Hit,
    Miss { evicted: bool, evicted_dirty: bool },
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

    pub fn access(&mut self, page: PageId, write: bool, pos: usize) -> Outcome {
        if let Some(&f) = self.page_table.get(&page) {
            if write {
                self.frames[f].dirty = true;
            }
            self.policy.on_hit(f, page, pos);
            return Outcome::Hit;
        }

        let f = if self.next_free < self.frames.len() {
            self.next_free += 1;
            self.next_free - 1
        } else {
            self.policy.victim()
        };

        let old = std::mem::replace(
            &mut self.frames[f],
            Frame {
                page: Some(page),
                dirty: write,
            },
        );
        let (evicted, evicted_dirty) = match old.page {
            Some(old_page) => {
                self.page_table.remove(&old_page);
                (true, old.dirty)
            }
            None => (false, false),
        };
        self.page_table.insert(page, f);
        self.policy.on_load(f, page, pos);
        Outcome::Miss {
            evicted,
            evicted_dirty,
        }
    }

    pub fn resident(&self, frame: FrameIdx) -> Option<(PageId, bool)> {
        let f = &self.frames[frame];
        f.page.map(|p| (p, f.dirty))
    }

    pub fn policy(&self) -> &dyn Policy {
        self.policy.as_ref()
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
    /// Recluster every N ticks; None disables reclustering.
    pub recluster_every: Option<u64>,
    /// Multiply co-access counts by this after each recluster (1.0 = never
    /// forget). Lower values adapt faster to workload drift.
    pub co_access_decay: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            frames: 64,
            policy: PolicyKind::Clock,
            recluster_every: Some(500),
            co_access_decay: 1.0,
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
    pub last_query_cost: f64,
    pub avg_cost_ema: f64,
    pub locality_ema: f64,
    pub reclusters: u64,
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

/// What happened on one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StepEvent {
    pub tick: u64,
    pub reclustered: bool,
}

pub struct Simulator {
    trace: Trace,
    cost: CostModel,
    cfg: SimConfig,
    pool: BufferPool,
    layout: Layout,
    co_access: CoAccess,
    next_query: usize,
    last_disk_pos: u64,
    last_reclustered: bool,
    metrics: Metrics,
}

impl Simulator {
    pub fn new(
        trace: Trace,
        initial_layout: Vec<TableId>,
        cost: CostModel,
        cfg: SimConfig,
    ) -> Self {
        assert!(cfg.frames > 0, "buffer pool needs at least one frame");
        let policy = cfg.policy.build(cfg.frames, &trace);
        let layout = Layout::new(initial_layout, trace.pages_per_table);
        Self {
            pool: BufferPool::new(cfg.frames, policy),
            trace,
            cost,
            cfg,
            layout,
            co_access: CoAccess::default(),
            next_query: 0,
            last_disk_pos: 0,
            last_reclustered: false,
            metrics: Metrics::default(),
        }
    }

    /// Convenience: build the scenario for `w`/`seed` and run it to the end.
    pub fn run(w: &Workload, seed: u64, ticks: u64, cfg: SimConfig) -> Self {
        let (trace, layout) = build_scenario(w, seed, ticks);
        let mut sim = Self::new(trace, layout, w.cost.clone(), cfg);
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

    pub fn layout(&self) -> &[TableId] {
        self.layout.order()
    }

    pub fn pool(&self) -> &BufferPool {
        &self.pool
    }

    /// Run the next query. Returns None when the trace is exhausted.
    pub fn step(&mut self) -> Option<StepEvent> {
        if self.is_done() {
            return None;
        }
        let q = self.next_query;
        self.next_query += 1;
        let t = q as u64 + 1;
        let range = self.trace.query_bounds(q);

        let touched: Vec<TableId> = self.trace.accesses()[range.clone()]
            .iter()
            .map(|a| self.trace.table_of(a.page))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.co_access.record(&touched);

        let mut query_cost = 0.0;
        for pos in range {
            let a = self.trace.accesses()[pos];
            query_cost += self.access(a, pos);
        }

        let reclustered = matches!(self.cfg.recluster_every, Some(k) if t % k == 0);
        if reclustered {
            let order = layout::optimal_order(self.trace.num_tables, &self.co_access);
            self.layout = Layout::new(order, self.trace.pages_per_table);
            self.co_access.decay(self.cfg.co_access_decay);
            self.metrics.reclusters += 1;
        }
        self.last_reclustered = reclustered;

        // Locality: mean slot distance between the tables this query touched.
        let locality = if touched.len() >= 2 {
            let mut sum = 0.0;
            let mut pairs = 0.0;
            for i in 0..touched.len() {
                for j in (i + 1)..touched.len() {
                    sum += self
                        .layout
                        .slot(touched[i])
                        .abs_diff(self.layout.slot(touched[j])) as f64;
                    pairs += 1.0;
                }
            }
            sum / pairs
        } else {
            0.0
        };

        let m = &mut self.metrics;
        if t == 1 {
            m.avg_cost_ema = query_cost;
            m.locality_ema = locality;
        } else {
            m.avg_cost_ema = EMA_ALPHA * query_cost + (1.0 - EMA_ALPHA) * m.avg_cost_ema;
            m.locality_ema = EMA_ALPHA * locality + (1.0 - EMA_ALPHA) * m.locality_ema;
        }
        m.last_query_cost = query_cost;
        m.total_cost += query_cost;

        Some(StepEvent {
            tick: t,
            reclustered,
        })
    }

    /// Serve one access; returns its cost.
    fn access(&mut self, a: Access, pos: usize) -> f64 {
        match self.pool.access(a.page, a.write, pos) {
            Outcome::Hit => {
                self.metrics.hits += 1;
                self.cost.hit
            }
            Outcome::Miss {
                evicted,
                evicted_dirty,
            } => {
                self.metrics.misses += 1;
                let disk_pos = self.layout.disk_pos(a.page);
                let seek_dist = disk_pos.abs_diff(self.last_disk_pos);
                let total_disk = self.trace.num_tables as u64 * self.trace.pages_per_table as u64;
                let seek_cost = (seek_dist as f64 / total_disk as f64) * self.cost.seek_max;
                self.last_disk_pos = disk_pos;

                let mut cost = self.cost.miss + seek_cost;
                if evicted {
                    self.metrics.evictions += 1;
                }
                if evicted_dirty {
                    self.metrics.dirty_evictions += 1;
                    cost += self.cost.dirty_writeback;
                }
                cost
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let m = &self.metrics;
        let policy = self.pool.policy();
        Snapshot {
            tick: self.tick(),
            recluster: self.last_reclustered,
            policy: self.cfg.policy.name().to_string(),
            frames: (0..self.pool.frames.len())
                .map(|i| match self.pool.resident(i) {
                    Some((page, dirty)) => FrameSnap {
                        page_id: page as i64,
                        table_id: self.trace.table_of(page) as i32,
                        dirty,
                        ref_bit: policy.ref_bit(i),
                    },
                    None => FrameSnap {
                        page_id: -1,
                        table_id: -1,
                        dirty: false,
                        ref_bit: false,
                    },
                })
                .collect(),
            metrics: MetricsSnap {
                hit_rate: m.hit_rate(),
                avg_cost: m.avg_cost_ema,
                query_cost: m.last_query_cost,
                total_cost: m.total_cost,
                hits: m.hits,
                misses: m.misses,
                evictions: m.evictions,
                dirty_evictions: m.dirty_evictions,
                locality_score: m.locality_ema,
            },
            disk_layout: self.layout.order().to_vec(),
        }
    }

    pub fn summary(&self) -> RunSummary {
        let m = &self.metrics;
        let queries = self.tick();
        RunSummary {
            policy: self.cfg.policy.name().to_string(),
            frames: self.cfg.frames,
            recluster_every: self.cfg.recluster_every,
            co_access_decay: self.cfg.co_access_decay,
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
            final_avg_cost_ema: m.avg_cost_ema,
            final_locality_ema: m.locality_ema,
            reclusters: m.reclusters,
            final_layout: self.layout.order().to_vec(),
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
                let (trace, layout) = build_scenario(&w, 3, 400);
                let cfg = SimConfig {
                    frames,
                    policy: kind,
                    ..SimConfig::default()
                };
                let mut sim = Simulator::new(trace, layout, w.cost.clone(), cfg);
                while sim.step().is_some() {
                    sim.pool()
                        .check_consistency()
                        .unwrap_or_else(|e| panic!("{kind} f={frames}: {e}"));
                }
            }
        }
    }
}
