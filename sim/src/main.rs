mod cli;

use std::fs;
use std::path::Path;

use bpsim::layout::MAX_RECLUSTER_TABLES;
use bpsim::run::RunFile;
use bpsim::{CapturedTrace, PolicyKind, SimConfig, Simulator, Workload, build_scenario};
use chrono::Local;
use clap::Parser;
use serde::Serialize;

use cli::Cli;

/// The viewer page; the CLI puts the run file where this marker is.
const VIEWER: &str = include_str!("../../viewer/index.html");
const RUN_SLOT: &str = "__BPSIM_RUN__";

/// Run files keep every frame for every access; past this many frame
/// entries (roughly 15 MB of JSON) a run file is refused.
const MAX_FRAME_ENTRIES: usize = 250_000;

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
enum Input {
    Trace(CapturedTrace),
    Workload(Workload),
}

impl Input {
    fn name(&self) -> &str {
        match self {
            Input::Trace(t) => &t.scenario,
            Input::Workload(w) => &w.name,
        }
    }

    fn mode(&self, args: &Cli) -> &'static str {
        match self {
            Input::Trace(_) => "replay",
            Input::Workload(_) if args.recluster => "recluster",
            Input::Workload(_) => "no_recluster",
        }
    }

    fn simulator(&self, policy: PolicyKind, args: &Cli) -> Simulator {
        let cfg = SimConfig {
            frames: args.frames,
            policy,
            recluster_every: args.recluster.then_some(args.recluster_every),
            co_access_decay: args.co_access_decay,
        };
        match self {
            Input::Trace(t) => {
                let mut sim = t.simulator(cfg);
                sim.record_events();
                sim
            }
            Input::Workload(w) => {
                let (trace, initial_layout) = build_scenario(w, args.seed, args.ticks);
                Simulator::new(trace, initial_layout, w.cost.clone(), cfg)
            }
        }
    }
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

fn load_trace(path: &Path, args: &Cli) -> Result<Input, String> {
    let captured =
        CapturedTrace::from_json(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    check_frames(args.frames, captured.page_count as u64, "trace")?;
    let entries = captured.to_trace().accesses().len() * args.frames * args.policy.0.len();
    if entries > MAX_FRAME_ENTRIES {
        return Err(format!(
            "the run file would hold {entries} frame entries (limit {MAX_FRAME_ENTRIES}); \
             use fewer --frames or policies"
        ));
    }
    Ok(Input::Trace(captured))
}

fn load_workload(args: &Cli) -> Result<Input, String> {
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
    Ok(Input::Workload(workload))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut s = serde_json::to_string(value).map_err(|e| e.to_string())?;
    s.push('\n');
    write(path, &s)
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn create_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("creating {}: {e}", path.display()))
}

/// The viewer page with the run file built in. Every `<` in the JSON becomes
/// `<` (they only occur inside strings) so text from the database, such
/// as `</script>`, cannot end the script element early.
fn viewer_html(run_json: &str) -> String {
    assert_eq!(VIEWER.matches(RUN_SLOT).count(), 1, "one data slot");
    VIEWER.replace(RUN_SLOT, &run_json.replace('<', "\\u003c"))
}

fn run(args: Cli) -> Result<(), String> {
    let input = match &args.trace {
        Some(path) => load_trace(path, &args)?,
        None => load_workload(&args)?,
    };
    let name = input.name().to_string();
    let mode = input.mode(&args);
    let policies = &args.policy.0;
    let compare = policies.len() > 1;

    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let folder = if compare {
        "compare"
    } else {
        policies[0].name()
    };
    let run_dir = args.out.join(&name).join(folder).join(mode).join(ts);
    let snapshot_every = args.snapshot_every.unwrap_or(match input {
        Input::Trace(_) => 1,
        Input::Workload(_) => 10,
    });

    let mut sims = Vec::with_capacity(policies.len());
    for &policy in policies {
        let mut sim = input.simulator(policy, &args);
        let dir = if compare {
            run_dir.join(policy.name())
        } else {
            run_dir.clone()
        };
        let snap_dir = dir.join("snapshots");
        create_dir(&snap_dir)?;

        while let Some(ev) = sim.step() {
            // The layout does not depend on the policy, so print it once.
            if ev.reclustered && sims.is_empty() {
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
            name,
            summary.policy,
            summary.frames,
            mode,
            summary.hit_rate,
            summary.mean_cost_per_query
        );
        let is_trace = matches!(input, Input::Trace(_));
        write_json(
            &dir.join("summary.json"),
            &SummaryFile {
                workload: (!is_trace).then_some(name.as_str()),
                trace: is_trace.then_some(name.as_str()),
                seed: (!is_trace).then_some(args.seed),
                run: summary,
            },
        )?;
        sims.push(sim);
    }

    if let Input::Trace(trace) = &input {
        let run_json =
            serde_json::to_string(&RunFile::new(trace, &sims)).map_err(|e| e.to_string())?;
        write(&run_dir.join("run.json"), &format!("{run_json}\n"))?;
        write(&run_dir.join("viewer.html"), &viewer_html(&run_json))?;
    }
    println!("Run written to {}", run_dir.display());
    Ok(())
}
