use serde::Serialize;

#[derive(Serialize)]
pub struct FrameSnap {
    pub page_id: i32,
    pub table_id: i32,
    pub dirty: bool,
    pub ref_bit: bool,
    pub pin_count: u16,
}

#[derive(Serialize)]
pub struct MetricsSnap {
    pub hit_rate: f64,
    pub avg_cost: f64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    pub locality_score: f64,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub tick: u64,
    pub recluster: bool,
    pub frames: Vec<FrameSnap>,
    pub metrics: MetricsSnap,
    pub table_order: Vec<u16>,
}
