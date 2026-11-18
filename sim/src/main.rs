mod cli;

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

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

/// Prints one line of progress. A reader that stops early (`sim ... | head -1`)
/// closes the pipe; that is not a failure, and the run still writes its files.
fn say(line: std::fmt::Arguments) {
    match writeln!(std::io::stdout(), "{line}") {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => eprintln!("warning: could not print: {e}"),
        _ => {}
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
    run: bpsim::summary::RunSummary,
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

    fn simulator(&self, policy: PolicyKind, args: &Cli) -> Simulator {
        let cfg = SimConfig {
            frames: args.frames,
            policy,
            storage: args.storage,
            read_ahead: args.read_ahead,
        };
        match self {
            Input::Trace(t) => {
                let mut sim = t.simulator(cfg);
                sim.record_events();
                sim
            }
            Input::Workload(w) => Simulator::new(build_scenario(w, args.seed, args.ticks), cfg),
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
    let policies = &args.policy.0;
    let compare = policies.len() > 1;

    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let folder = if compare {
        "compare"
    } else {
        policies[0].name()
    };
    let run_dir = args.out.join(&name).join(folder).join(ts);

    let mut sims = Vec::with_capacity(policies.len());
    for &policy in policies {
        let mut sim = input.simulator(policy, &args);
        let dir = if compare {
            run_dir.join(policy.name())
        } else {
            run_dir.clone()
        };
        create_dir(&dir)?;
        while sim.step().is_some() {}

        let summary = sim.summary();
        say(format_args!(
            "{} | {} | frames {} | {} | hit rate {:.3} | {} disk requests, {} pages read ahead | {:.3} ms/query",
            name,
            summary.policy,
            summary.frames,
            summary.storage,
            summary.hit_rate,
            summary.read_requests,
            summary.pages_read_ahead,
            summary.mean_ms_per_query
        ));
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
    say(format_args!("Run written to {}", run_dir.display()));
    Ok(())
}
