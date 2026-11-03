mod cli;

use std::fs;
use std::path::Path;

use bpsim::layout::MAX_RECLUSTER_TABLES;
use bpsim::{CapturedTrace, SimConfig, Simulator, Workload, build_scenario};
use chrono::Local;
use clap::Parser;
use serde::Serialize;

use cli::Cli;

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[derive(Serialize)]
struct SummaryFile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    workload: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(flatten)]
    run: bpsim::snapshot::RunSummary,
}

/// What a run replays: a seeded synthetic workload or a captured SQLite trace.
struct Source {
    name: String,
    mode: &'static str,
    is_trace: bool,
    sim: Simulator,
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn check_frames(frames: usize, total_pages: u64, what: &str) -> Result<(), String> {
    if frames as u64 > total_pages {
        return Err(format!(
            "--frames ({frames}) exceeds the {what}'s total pages ({total_pages}); the extra frames would never be used"
        ));
    }
    Ok(())
}

fn from_trace(path: &Path, args: &Cli) -> Result<Source, String> {
    let captured =
        CapturedTrace::from_json(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    check_frames(args.frames, captured.page_count as u64, "trace")?;
    let cfg = SimConfig {
        frames: args.frames,
        policy: args.policy,
        recluster_every: None,
        co_access_decay: args.co_access_decay,
    };
    Ok(Source {
        name: captured.scenario.clone(),
        mode: "replay",
        is_trace: true,
        sim: captured.simulator(cfg),
    })
}

fn from_workload(args: &Cli) -> Result<Source, String> {
    let workload = match &args.workload {
        Some(path) => {
            Workload::from_toml(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?
        }
        None => Workload::builtin_default(),
    };
    check_frames(args.frames, workload.total_pages(), "workload")?;
    if args.recluster && workload.num_tables > MAX_RECLUSTER_TABLES {
        return Err(format!(
            "reclustering is brute force (n!) and supports at most {MAX_RECLUSTER_TABLES} tables; \
             this workload has {}. Use --recluster=false",
            workload.num_tables
        ));
    }
    let cfg = SimConfig {
        frames: args.frames,
        policy: args.policy,
        recluster_every: args.recluster.then_some(args.recluster_every),
        co_access_decay: args.co_access_decay,
    };
    let (trace, initial_layout) = build_scenario(&workload, args.seed, args.ticks);
    Ok(Source {
        mode: if args.recluster {
            "recluster"
        } else {
            "no_recluster"
        },
        name: workload.name,
        is_trace: false,
        sim: Simulator::new(trace, initial_layout, workload.cost.clone(), cfg),
    })
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut s = serde_json::to_string(value).map_err(|e| e.to_string())?;
    s.push('\n');
    fs::write(path, s).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn run(args: Cli) -> Result<(), String> {
    let Source {
        name,
        mode,
        is_trace,
        mut sim,
    } = match &args.trace {
        Some(path) => from_trace(path, &args)?,
        None => from_workload(&args)?,
    };

    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let run_dir = args
        .out
        .join(&name)
        .join(sim.summary().policy)
        .join(mode)
        .join(ts);
    let snapshot_every = args.snapshot_every.unwrap_or(if is_trace { 1 } else { 10 });
    let snap_dir = run_dir.join("snapshots");
    fs::create_dir_all(&snap_dir).map_err(|e| format!("creating {}: {e}", snap_dir.display()))?;

    while let Some(ev) = sim.step() {
        if ev.reclustered {
            println!("tick {}: recluster → order: {:?}", ev.tick, sim.layout());
        }
        if snapshot_every > 0 && ev.tick % snapshot_every == 0 {
            write_json(
                &snap_dir.join(format!("{:06}.json", ev.tick)),
                &sim.snapshot(),
            )?;
        }
    }

    let summary = sim.summary();
    println!(
        "{} | {} | frames {} | {} | hit rate {:.3} | mean cost/query {:.2}",
        name, summary.policy, summary.frames, mode, summary.hit_rate, summary.mean_cost_per_query
    );
    write_json(
        &run_dir.join("summary.json"),
        &SummaryFile {
            workload: (!is_trace).then_some(name.as_str()),
            trace: is_trace.then_some(name.as_str()),
            seed: (!is_trace).then_some(args.seed),
            run: summary,
        },
    )?;
    println!("Run written to {}", run_dir.display());
    Ok(())
}
