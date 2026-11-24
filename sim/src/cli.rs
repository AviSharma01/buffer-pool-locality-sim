use std::path::PathBuf;

use bpsim::sim::ReadAhead;
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

    /// Read ahead on a miss that continues a table or index (PostgreSQL-style:
    /// on scans): auto (a quarter of the pool, at most 8 pages), off, or a page
    /// count. Optimal never reads ahead.
    #[arg(long, default_value = "auto", value_name = "auto|off|N")]
    pub read_ahead: ReadAhead,

    /// Buffer pool size in frames. A comma-separated list, or `sweep` for
    /// sizes from 8 frames to the whole database, runs a sweep instead: every
    /// policy at every size, written as sweep.json and a page of curves,
    /// sweep.html.
    #[arg(long, default_value = "64", value_parser = frames, value_name = "N|N,N,...|sweep")]
    pub frames: Frames,

    /// Write only summaries for a `--trace` run: no run.json or viewer.html,
    /// so a trace of any length can run.
    #[arg(long)]
    pub no_viewer: bool,

    /// Output root. Runs go to <out>/<name>/<policy>/<timestamp>/summary.json,
    /// with `compare` for the policy and one subfolder per policy when several
    /// run, and sweeps to <out>/<name>/sweep/<timestamp>/. `--trace` runs also
    /// write run.json and viewer.html.
    #[arg(long, default_value = "../outputs/runs")]
    pub out: PathBuf,
}

/// One pool size, or several for a sweep.
#[derive(Clone, Debug, PartialEq)]
pub enum Frames {
    One(usize),
    /// In increasing order.
    List(Vec<usize>),
    /// The default sizes for the input (see `bpsim::sweep::default_sizes`).
    Sweep,
}

fn frames(s: &str) -> Result<Frames, String> {
    if s.eq_ignore_ascii_case("sweep") {
        return Ok(Frames::Sweep);
    }
    let mut list = s
        .split(',')
        .map(|n| positive_usize(n.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    if let [one] = list[..] {
        return Ok(Frames::One(one));
    }
    list.sort_unstable();
    if let Some(w) = list.windows(2).find(|w| w[0] == w[1]) {
        return Err(format!("{} frames is listed twice", w[0]));
    }
    Ok(Frames::List(list))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_one_size_a_sorted_list_or_a_sweep() {
        assert_eq!(frames("64"), Ok(Frames::One(64)));
        assert_eq!(frames("256, 8,64"), Ok(Frames::List(vec![8, 64, 256])));
        assert_eq!(frames("Sweep"), Ok(Frames::Sweep));
        assert!(frames("8,0").unwrap_err().contains("at least 1"));
        assert!(frames("8,16,8").unwrap_err().contains("listed twice"));
        assert!(frames("eight").is_err());
    }
}
