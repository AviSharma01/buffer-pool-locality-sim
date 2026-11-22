mod cli;

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use bpsim::run::RunFile;
use bpsim::{CapturedTrace, PolicyKind, SimConfig, Simulator, Workload, build_scenario};
use chrono::Local;
use clap::Parser;
use serde::Serialize;

use cli::Cli;

/// The viewer page; the CLI puts the run file where this marker is.
const VIEWER: &str = include_str!("../../viewer/index.html");
const RUN_SLOT: &str = "__BPSIM_RUN__";
const EXAMPLE_SLOT: &str = "__BPSIM_EXAMPLE__";

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

/// The most a gzipped file may expand to: well above any capture, far below
/// what would exhaust memory.
const MAX_UNZIPPED: u64 = 1 << 30;

/// A text file, or a gzipped one if its name ends in `.gz`.
fn read(path: &Path) -> Result<String, String> {
    read_limited(path, MAX_UNZIPPED)
}

fn read_limited(path: &Path, max_unzipped: u64) -> Result<String, String> {
    let fail = |e: std::io::Error| format!("reading {}: {e}", path.display());
    if path.extension().is_some_and(|x| x == "gz") {
        use std::io::Read;
        let file = fs::File::open(path).map_err(fail)?;
        let mut text = String::new();
        flate2::read::GzDecoder::new(file)
            .take(max_unzipped + 1)
            .read_to_string(&mut text)
            .map_err(fail)?;
        if text.len() as u64 > max_unzipped {
            return Err(format!(
                "{}: unzips to more than {} MiB",
                path.display(),
                max_unzipped >> 20
            ));
        }
        return Ok(text);
    }
    fs::read_to_string(path).map_err(fail)
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
    let accesses = captured.to_trace().accesses().len();
    let entries = accesses * args.frames * args.policy.0.len();
    if entries > MAX_FRAME_ENTRIES {
        return Err(format!(
            "too long to step through in the viewer: {accesses} page requests x {} frames x {} \
             polic{} would be {entries} frame entries (the viewer takes up to \
             {MAX_FRAME_ENTRIES}); {}",
            args.frames,
            args.policy.0.len(),
            if args.policy.0.len() == 1 { "y" } else { "ies" },
            if accesses > MAX_FRAME_ENTRIES {
                "the trace is too long for the viewer even with one frame and one policy"
            } else {
                "use fewer --frames or policies, or a shorter trace"
            }
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

/// A new folder `parent/ts`, or `parent/ts-2`, `-3`, ... if runs started in
/// the same second: an earlier run is never overwritten.
fn new_run_dir(parent: &Path, ts: &str) -> Result<PathBuf, String> {
    create_dir(parent)?;
    for n in 1.. {
        let dir = if n == 1 {
            parent.join(ts)
        } else {
            parent.join(format!("{ts}-{n}"))
        };
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("creating {}: {e}", dir.display())),
        }
    }
    unreachable!("some suffix is free")
}

fn create_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("creating {}: {e}", path.display()))
}

/// The viewer page with the run file built in, and the worked example for its
/// "How it works". Every `<` in the JSON becomes `\u003c` (they only occur
/// inside strings) so text from the database, such as `</script>`, cannot end
/// the script element early.
fn viewer_html(run_json: &str) -> String {
    assert_eq!(VIEWER.matches(RUN_SLOT).count(), 1, "one data slot");
    assert_eq!(VIEWER.matches(EXAMPLE_SLOT).count(), 1, "one example slot");
    let safe = |json: &str| json.replace('<', "\\u003c");
    VIEWER
        .replace(EXAMPLE_SLOT, &safe(&bpsim::example::run_json()))
        .replace(RUN_SLOT, &safe(run_json))
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
    let run_dir = new_run_dir(&args.out.join(&name).join(folder), &ts)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_gzipped_file_that_unzips_too_far_is_refused() {
        let path = std::env::temp_dir().join(format!("bpsim-big-{}.json.gz", std::process::id()));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&vec![b' '; 3 << 20]).unwrap();
        fs::write(&path, gz.finish().unwrap()).unwrap();
        let err = read_limited(&path, 2 << 20).unwrap_err();
        assert!(err.contains("unzips to more than 2 MiB"), "{err}");
        assert_eq!(read_limited(&path, 4 << 20).unwrap().len(), 3 << 20);
        fs::remove_file(path).unwrap();
    }
}
