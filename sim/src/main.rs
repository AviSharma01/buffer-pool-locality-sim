mod cli;

use std::fs;
use std::path::Path;

use bpsim::layout::MAX_RECLUSTER_TABLES;
use bpsim::{SimConfig, Simulator, Workload, build_scenario};
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
    workload: &'a str,
    seed: u64,
    #[serde(flatten)]
    run: bpsim::snapshot::RunSummary,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut s = serde_json::to_string(value).map_err(|e| e.to_string())?;
    s.push('\n');
    fs::write(path, s).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn run(args: Cli) -> Result<(), String> {
    let workload = match &args.workload {
        Some(path) => {
            let text =
                fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
            Workload::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?
        }
        None => Workload::builtin_default(),
    };

    if args.frames as u64 > workload.total_pages() {
        return Err(format!(
            "--frames ({}) exceeds the workload's total pages ({}); the extra frames would never be used",
            args.frames,
            workload.total_pages()
        ));
    }
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
    let mut sim = Simulator::new(trace, initial_layout, workload.cost.clone(), cfg);

    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let mode = if args.recluster {
        "recluster"
    } else {
        "no_recluster"
    };
    let run_dir = args
        .out
        .join(&workload.name)
        .join(sim.summary().policy)
        .join(mode)
        .join(ts);
    let snap_dir = run_dir.join("snapshots");
    fs::create_dir_all(&snap_dir).map_err(|e| format!("creating {}: {e}", snap_dir.display()))?;

    while let Some(ev) = sim.step() {
        if ev.reclustered {
            println!("tick {}: recluster → order: {:?}", ev.tick, sim.layout());
        }
        if args.snapshot_every > 0 && ev.tick % args.snapshot_every == 0 {
            write_json(
                &snap_dir.join(format!("{:06}.json", ev.tick)),
                &sim.snapshot(),
            )?;
        }
    }

    let summary = sim.summary();
    println!(
        "{} | {} | frames {} | {} | hit rate {:.3} | mean cost/query {:.2}",
        workload.name,
        summary.policy,
        summary.frames,
        mode,
        summary.hit_rate,
        summary.mean_cost_per_query
    );
    write_json(
        &run_dir.join("summary.json"),
        &SummaryFile {
            workload: &workload.name,
            seed: args.seed,
            run: summary,
        },
    )?;
    println!("Run written to {}", run_dir.display());
    Ok(())
}
