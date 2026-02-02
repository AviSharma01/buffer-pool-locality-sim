#[derive(Clone)]
pub struct Config {
    pub ticks: u64,
    pub frames: usize,
    pub snapshot_every: u64,
    pub recluster_every: u64,
    pub seed: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ticks: 5000,
            frames: 64,
            snapshot_every: 10,
            recluster_every: 500,
            seed: 42,
        }
    }
}
