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

    /// Replay a trace captured from SQLite (traces/*.trace.json) instead of a
    /// workload. `--ticks`, `--seed` and the recluster flags do not apply.
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

    /// Ticks between snapshot files; 0 disables them (summary only).
    /// Defaults to every tick for `--trace` and every 10 ticks for workloads.
    #[arg(long)]
    pub snapshot_every: Option<u64>,

    /// Output root. Runs go to <out>/<name>/<policy>/<mode>/<timestamp>/, with
    /// `compare` for the policy and one subfolder per policy when several run.
    /// `--trace` runs also write run.json and viewer.html there.
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

fn unit_interval(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if v > 0.0 && v <= 1.0 {
        Ok(v)
    } else {
        Err("must be in (0, 1]".into())
    }
}
