use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::snapshot::{FrameSnap, MetricsSnap, Snapshot};

pub struct Simulator {
    rng: StdRng,
    frames: Vec<FrameState>,
    hits: u64,
    misses: u64,
    evictions: u64,
    dirty_evictions: u64,
}

#[derive(Clone)]
struct FrameState {
    table_id: i32,
    dirty: bool,
    ref_bit: bool,
}

impl Simulator {
    pub fn new(seed: u64, num_frames: usize) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut frames = Vec::with_capacity(num_frames);
        for _ in 0..num_frames {
            // start empty
            frames.push(FrameState {
                table_id: -1,
                dirty: false,
                ref_bit: false,
            });
        }
        // sprinkle a few initial pages so the first GIF isn't blank (optional)
        for i in 0..(num_frames / 4) {
            frames[i].table_id = rng.gen_range(0..6);
            frames[i].ref_bit = true;
        }

        Self { rng, frames, hits: 0, misses: 0, evictions: 0, dirty_evictions: 0 }
    }

    pub fn tick(&mut self, t: u64) -> (Snapshot, f64) {
        // Dummy “workload”: randomly touch a table id
        let table = self.rng.gen_range(0..6) as i32;

        // Dummy “hit” if table already exists somewhere in frames
        let hit = self.frames.iter().any(|f| f.table_id == table);
        let cost = if hit {
            self.hits += 1;
            1.0
        } else {
            self.misses += 1;
            let victim = self.rng.gen_range(0..self.frames.len());
            if self.frames[victim].dirty {
                self.dirty_evictions += 1;
            }
            self.evictions += 1;

            self.frames[victim] = FrameState {
                table_id: table,
                dirty: self.rng.gen_bool(0.1), // 10% writes
                ref_bit: true,
            };
            10.0
        };

        // decay ref_bits a little so it changes visually
        if t % 25 == 0 {
            for f in &mut self.frames {
                f.ref_bit = false;
            }
        }

        let total = self.hits + self.misses;
        let hit_rate = if total == 0 { 0.0 } else { self.hits as f64 / total as f64 };

        let snap = Snapshot {
            tick: t,
            recluster: false,
            frames: self.frames.iter().map(|f| FrameSnap {
                table_id: f.table_id,
                dirty: f.dirty,
                ref_bit: f.ref_bit,
            }).collect(),
            metrics: MetricsSnap {
                hit_rate,
                avg_cost: cost,         
                evictions: self.evictions,
                dirty_evictions: self.dirty_evictions,
                locality_score: 0.0,     // placeholder for now
            },
        };

        (snap, cost)
    }
}
