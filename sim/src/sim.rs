use std::collections::{HashMap, HashSet};

use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::snapshot::{FrameSnap, MetricsSnap, Snapshot};

type PageId = u32;
type TableId = u16;
type FrameIdx = usize;

#[derive(Clone)]
struct Frame {
    page_id: Option<PageId>,
    table_id: Option<TableId>,
    dirty: bool,
    ref_bit: bool,
    pin_count: u16,
}

pub struct Simulator {
    rng: StdRng,

    num_tables: u16,
    pages_per_table: u32,

    frames: Vec<Frame>,
    page_table: HashMap<PageId, FrameIdx>,
    clock_hand: FrameIdx,

    hits: u64,
    misses: u64,
    evictions: u64,
    dirty_evictions: u64,

    avg_cost_ema: f64,
    locality_score_ema: f64,

    co_access: HashMap<(TableId, TableId), u64>,
    pub table_order: Vec<TableId>,

    // Physical layout of tables on disk (slot index → TableId).
    // Updated on every recluster event; stays sequential otherwise.
    disk_layout: Vec<TableId>,
    last_disk_pos: u64,
}

impl Simulator {
    pub fn new(seed: u64, num_frames: usize, num_tables: u16, pages_per_table: u32) -> Self {
        let rng = StdRng::seed_from_u64(seed);

        let frames = (0..num_frames)
            .map(|_| Frame {
                page_id: None,
                table_id: None,
                dirty: false,
                ref_bit: false,
                pin_count: 0,
            })
            .collect();

        let table_order = (0..num_tables).collect();

        Self {
            rng,
            num_tables,
            pages_per_table,
            frames,
            page_table: HashMap::new(),
            clock_hand: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            dirty_evictions: 0,
            avg_cost_ema: 0.0,
            locality_score_ema: 0.0,
            co_access: HashMap::new(),
            table_order,
            disk_layout: (0..num_tables).collect(),
            last_disk_pos: 0,
        }
    }

    fn make_page_id(&self, table: TableId, page_idx: u32) -> PageId {
        (table as u32) * self.pages_per_table + page_idx
    }

    fn table_of(&self, page_id: PageId) -> TableId {
        (page_id / self.pages_per_table) as TableId
    }

    /// Physical disk address of a page under the current disk_layout.
    fn disk_pos_of(&self, page_id: PageId) -> u64 {
        let table = self.table_of(page_id);
        let page_within = (page_id % self.pages_per_table) as u64;
        let slot = self.disk_layout.iter().position(|&t| t == table).unwrap_or(0) as u64;
        slot * self.pages_per_table as u64 + page_within
    }

    fn sample_query_pages(&mut self) -> Vec<PageId> {
        let roll: f64 = self.rng.gen_range(0.0..1.0);
        let (a, b): (TableId, TableId) = if roll < 0.40 {
            (0, 1)
        } else if roll < 0.70 {
            (0, 2)
        } else if roll < 0.90 {
            (3, 4)
        } else {
            (5, 0)
        };

        let hot = self.rng.gen_bool(0.70);
        let max_idx = if hot {
            (self.pages_per_table / 10).max(1)
        } else {
            self.pages_per_table
        };

        let a_n = self.rng.gen_range(2..6);
        let b_n = self.rng.gen_range(2..6);

        let mut pages = Vec::with_capacity((a_n + b_n + 8) as usize);

        for _ in 0..a_n {
            let idx = self.rng.gen_range(0..max_idx);
            pages.push(self.make_page_id(a, idx));
        }
        for _ in 0..b_n {
            let idx = self.rng.gen_range(0..max_idx);
            pages.push(self.make_page_id(b, idx));
        }

        if self.rng.gen_bool(0.15) {
            let t = self.rng.gen_range(0..self.num_tables) as TableId;
            let start = self
                .rng
                .gen_range(0..self.pages_per_table.saturating_sub(8).max(1));
            for i in 0..8 {
                pages.push(self.make_page_id(t, start + i));
            }
        }

        pages
    }

    fn pick_victim_clock(&mut self) -> FrameIdx {
        let n = self.frames.len();
        loop {
            let idx = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % n;

            if self.frames[idx].page_id.is_none() {
                return idx;
            }
            if self.frames[idx].pin_count > 0 {
                continue;
            }
            if self.frames[idx].ref_bit {
                self.frames[idx].ref_bit = false;
                continue;
            }
            return idx;
        }
    }

    fn access_page(&mut self, page_id: PageId) -> f64 {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            self.hits += 1;
            let f = &mut self.frames[frame_idx];
            f.ref_bit = true;
            if self.rng.gen_bool(0.05) {
                f.dirty = true;
            }
            return 1.0;
        }

        self.misses += 1;

        // Seek cost: proportional to disk distance from last fetched page.
        let disk_pos = self.disk_pos_of(page_id);
        let seek_dist = (disk_pos as i64 - self.last_disk_pos as i64).unsigned_abs();
        let total_disk = self.num_tables as u64 * self.pages_per_table as u64;
        let seek_cost = (seek_dist as f64 / total_disk as f64) * 30.0;
        self.last_disk_pos = disk_pos;

        let victim = self.pick_victim_clock();
        let mut cost = 10.0 + seek_cost;

        if let Some(old_pid) = self.frames[victim].page_id {
            self.evictions += 1;
            self.page_table.remove(&old_pid);
            if self.frames[victim].dirty {
                self.dirty_evictions += 1;
                cost += 20.0;
            }
        }

        self.frames[victim] = Frame {
            page_id: Some(page_id),
            table_id: Some(self.table_of(page_id)),
            dirty: self.rng.gen_bool(0.05),
            ref_bit: true,
            pin_count: 0,
        };
        self.page_table.insert(page_id, victim);

        cost
    }

    fn recompute_table_order(&mut self) {
        let mut remaining: Vec<TableId> = (0..self.num_tables).collect();
        let mut order: Vec<TableId> = Vec::with_capacity(self.num_tables as usize);

        // Start from the table with the highest total co-access weight.
        let hottest = remaining
            .iter()
            .copied()
            .max_by_key(|&t| {
                (0..self.num_tables)
                    .filter(|&o| o != t)
                    .map(|o| {
                        let a = t.min(o);
                        let b = t.max(o);
                        self.co_access.get(&(a, b)).copied().unwrap_or(0)
                    })
                    .sum::<u64>()
            })
            .unwrap();

        order.push(hottest);
        remaining.retain(|&t| t != hottest);

        while !remaining.is_empty() {
            let last = *order.last().unwrap();
            let next = remaining
                .iter()
                .copied()
                .max_by_key(|&t| {
                    let a = last.min(t);
                    let b = last.max(t);
                    self.co_access.get(&(a, b)).copied().unwrap_or(0)
                })
                .unwrap();
            order.push(next);
            remaining.retain(|&t| t != next);
        }

        self.disk_layout = order.clone();
        self.table_order = order;
    }

    pub fn tick(&mut self, t: u64, recluster: bool) -> Snapshot {
        let pages = self.sample_query_pages();

        let touched: Vec<TableId> = pages
            .iter()
            .map(|&pid| self.table_of(pid))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for i in 0..touched.len() {
            for j in (i + 1)..touched.len() {
                let a = touched[i].min(touched[j]);
                let b = touched[i].max(touched[j]);
                *self.co_access.entry((a, b)).or_insert(0) += 1;
            }
        }

        let mut cost_sum = 0.0;
        for pid in pages {
            cost_sum += self.access_page(pid);
        }

        if recluster {
            self.recompute_table_order();
            println!("tick {t}: recluster → order: {:?}", self.table_order);
        }

        // Locality score: average pairwise distance between touched tables in
        // the current table_order. Lower means co-accessed tables sit closer
        // together → better locality.
        let pos: HashMap<TableId, usize> = self
            .table_order
            .iter()
            .enumerate()
            .map(|(i, &t)| (t, i))
            .collect();
        let locality_tick = if touched.len() >= 2 {
            let pairs = (touched.len() * (touched.len() - 1) / 2) as f64;
            let dist_sum: f64 = (0..touched.len())
                .flat_map(|i| (i + 1..touched.len()).map(move |j| (i, j)))
                .map(|(i, j)| (pos[&touched[i]] as f64 - pos[&touched[j]] as f64).abs())
                .sum();
            dist_sum / pairs
        } else {
            0.0
        };

        let alpha = 0.02;
        if t == 1 {
            self.avg_cost_ema = cost_sum;
            self.locality_score_ema = locality_tick;
        } else {
            self.avg_cost_ema = alpha * cost_sum + (1.0 - alpha) * self.avg_cost_ema;
            self.locality_score_ema =
                alpha * locality_tick + (1.0 - alpha) * self.locality_score_ema;
        }

        let total = self.hits + self.misses;
        let hit_rate = if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        };

        Snapshot {
            tick: t,
            recluster,
            frames: self
                .frames
                .iter()
                .map(|f| FrameSnap {
                    page_id: f.page_id.map(|x| x as i32).unwrap_or(-1),
                    table_id: f.table_id.map(|x| x as i32).unwrap_or(-1),
                    dirty: f.dirty,
                    ref_bit: f.ref_bit,
                    pin_count: f.pin_count,
                })
                .collect(),
            metrics: MetricsSnap {
                hit_rate,
                avg_cost: self.avg_cost_ema,
                evictions: self.evictions,
                dirty_evictions: self.dirty_evictions,
                locality_score: self.locality_score_ema,
            },
            table_order: self.table_order.clone(),
            disk_layout: self.disk_layout.clone(),
        }
    }
}
