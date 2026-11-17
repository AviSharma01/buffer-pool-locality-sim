use std::path::PathBuf;

use bpsim::{PolicyKind, StorageKind};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about = "Buffer pool simulator")]
pub struct Cli {
    /// Workload TOML file (see sim/workloads/). Defaults to the built-in
    /// `default` workload.
    #[arg(long)]
    pub workload: Option<PathBuf>,

    /// Replay a trace captured from SQLite (traces/*.trace.json) instead of a
    /// workload. `--ticks` and `--seed` do not apply.
    #[arg(long, conflicts_with = "workload")]
    pub trace: Option<PathBuf>,

    /// Page replacement policy: fifo, lru, clock, pg (PostgreSQL-style),
    /// innodb (InnoDB-style) or opt. A comma-separated list, or `all`, runs
    /// several on the same input.
    #[arg(long, default_value = "clock", value_parser = policies)]
    pub policy: Policies,

    /// RNG seed. Same seed = same trace, so runs are directly comparable.
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Number of queries to simulate (ignored by explicit-trace workloads).
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(1..))]
    pub ticks: u64,

    /// Storage to price disk reads and write-backs with: ssd or hdd. It never
    /// changes what is cached; the viewer can switch between them.
    #[arg(long, default_value = "ssd")]
    pub storage: StorageKind,

    /// Buffer pool size in frames.
    #[arg(long, default_value_t = 64, value_parser = positive_usize)]
    pub frames: usize,

    /// Output root. Runs go to <out>/<name>/<policy>/<timestamp>/summary.json,
    /// with `compare` for the policy and one subfolder per policy when several
    /// run. `--trace` runs also write run.json and viewer.html there.
    #[arg(long, default_value = "../outputs/runs")]
    pub out: PathBuf,
}

/// The policies to run, in the order given.
#[derive(Clone, Debug)]
pub struct Policies(pub Vec<PolicyKind>);

fn policies(s: &str) -> Result<Policies, String> {
    if s.eq_ignore_ascii_case("all") {
        return Ok(Policies(PolicyKind::ALL.to_vec()));
    }
    let mut list = Vec::new();
    for name in s.split(',') {
        let kind: PolicyKind = name.trim().parse()?;
        if list.contains(&kind) {
            return Err(format!("policy {kind} is listed twice"));
        }
        list.push(kind);
    }
    Ok(Policies(list))
}

fn positive_usize(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1".into()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}
