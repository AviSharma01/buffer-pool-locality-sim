use std::path::PathBuf;

use bpsim::PolicyKind;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about = "Buffer pool + disk layout simulator")]
pub struct Cli {
    /// Workload TOML file (see sim/workloads/). Defaults to the built-in
    /// `default` workload.
    #[arg(long)]
    pub workload: Option<PathBuf>,

    /// Page replacement policy: fifo, lru, clock, 2q or opt.
    #[arg(long, default_value = "clock")]
    pub policy: PolicyKind,

    /// RNG seed. Same seed = same trace, so runs are directly comparable.
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Number of queries to simulate (ignored by explicit-trace workloads).
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(1..))]
    pub ticks: u64,

    /// Buffer pool size in frames.
    #[arg(long, default_value_t = 64, value_parser = positive_usize)]
    pub frames: usize,

    /// Enable periodic reclustering of tables on disk.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub recluster: bool,

    /// Ticks between recluster events.
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(1..))]
    pub recluster_every: u64,

    /// Multiply co-access history by this after each recluster, in (0, 1].
    /// 1.0 never forgets; lower adapts faster to workload drift.
    #[arg(long, default_value_t = 1.0, value_parser = unit_interval)]
    pub co_access_decay: f64,

    /// Ticks between snapshot files. 0 disables snapshots (summary only).
    #[arg(long, default_value_t = 10)]
    pub snapshot_every: u64,

    /// Output root. Runs go to <out>/<workload>/<policy>/<recluster|no_recluster>/<timestamp>/.
    #[arg(long, default_value = "../outputs/runs")]
    pub out: PathBuf,
}

fn positive_usize(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1".into()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

fn unit_interval(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if v > 0.0 && v <= 1.0 {
        Ok(v)
    } else {
        Err("must be in (0, 1]".into())
    }
}
