mod cli;

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bpsim::lru_curve::LruCurve;
use bpsim::run::RunFile;
use bpsim::sweep::{self, SweepFile};
use bpsim::trace::Trace;
use bpsim::{CapturedTrace, PolicyKind, SimConfig, Simulator, Workload, build_scenario};
use chrono::Local;
use clap::Parser;
use serde::Serialize;

use cli::{Cli, Frames};

/// The viewer page; the CLI puts the run file where this marker is.
const VIEWER: &str = include_str!("../../viewer/index.html");
const RUN_SLOT: &str = "__BPSIM_RUN__";
const EXAMPLE_SLOT: &str = "__BPSIM_EXAMPLE__";

/// The sweep page; the CLI puts the sweep file where this marker is.
const SWEEP_PAGE: &str = include_str!("../../viewer/sweep.html");
const SWEEP_SLOT: &str = "__BPSIM_SWEEP__";

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

    /// "trace" or "workload", for messages and the sweep file.
    fn kind(&self) -> &'static str {
        match self {
            Input::Trace(_) => "trace",
            Input::Workload(_) => "workload",
        }
    }

    /// Pages in the database: the largest pool worth simulating.
    fn pages(&self) -> u64 {
        match self {
            Input::Trace(t) => u64::from(t.page_count),
            Input::Workload(w) => w.total_pages(),
        }
    }

    fn trace(&self, args: &Cli) -> Trace {
        match self {
            Input::Trace(t) => t.to_trace(),
            Input::Workload(w) => build_scenario(w, args.seed, args.ticks),
        }
    }

    /// Whether this run writes run.json and the step-through viewer.
    fn has_viewer(&self, args: &Cli) -> bool {
        matches!(self, Input::Trace(_)) && !args.no_viewer
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

fn check_frames(frames: usize, input: &Input) -> Result<(), String> {
    let pages = input.pages();
    if frames as u64 > pages {
        return Err(format!(
            "--frames ({frames}) exceeds the {}'s total pages ({pages}); the extra frames would never be used",
            input.kind()
        ));
    }
    Ok(())
}

/// Run files keep every frame for every access, so a trace run with the
/// viewer must stay under [`MAX_FRAME_ENTRIES`].
fn check_viewer_size(trace: &CapturedTrace, frames: usize, policies: usize) -> Result<(), String> {
    let accesses = trace.to_trace().accesses().len();
    let entries = accesses * frames * policies;
    if entries <= MAX_FRAME_ENTRIES {
        return Ok(());
    }
    Err(format!(
        "too long to step through in the viewer: {accesses} page requests x {frames} frames x \
         {policies} polic{} would be {entries} frame entries (the viewer takes up to \
         {MAX_FRAME_ENTRIES}); {}, or add --no-viewer for summaries only",
        if policies == 1 { "y" } else { "ies" },
        if accesses > MAX_FRAME_ENTRIES {
            "the trace is too long for the viewer even with one frame and one policy"
        } else {
            "use fewer --frames or policies, or a shorter trace"
        }
    ))
}

fn load_trace(path: &Path) -> Result<Input, String> {
    let captured =
        CapturedTrace::from_json(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Input::Trace(captured))
}

fn load_workload(args: &Cli) -> Result<Input, String> {
    let workload = match &args.workload {
        Some(path) => {
            Workload::from_toml(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?
        }
        None => Workload::builtin_default(),
    };
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

/// JSON made safe to put inside a script element: every `<` becomes
/// `\u003c` (they only occur inside strings), so text from the database,
/// such as `</script>`, cannot end the element early.
fn script_safe(json: &str) -> String {
    json.replace('<', "\\u003c")
}

/// The viewer page with the run file built in, and the worked example for its
/// "How it works".
fn viewer_html(run_json: &str) -> String {
    assert_eq!(VIEWER.matches(RUN_SLOT).count(), 1, "one data slot");
    assert_eq!(VIEWER.matches(EXAMPLE_SLOT).count(), 1, "one example slot");
    VIEWER
        .replace(EXAMPLE_SLOT, &script_safe(&bpsim::example::run_json()))
        .replace(RUN_SLOT, &script_safe(run_json))
}

/// The sweep page with the sweep file built in.
fn sweep_html(sweep_json: &str) -> String {
    assert_eq!(SWEEP_PAGE.matches(SWEEP_SLOT).count(), 1, "one data slot");
    SWEEP_PAGE.replace(SWEEP_SLOT, &script_safe(sweep_json))
}

fn run(args: Cli) -> Result<(), String> {
    let input = match &args.trace {
        Some(path) => load_trace(path)?,
        None => load_workload(&args)?,
    };
    match &args.frames {
        Frames::One(frames) => run_policies(&input, *frames, &args),
        Frames::List(sizes) => run_sweep(&input, sizes.clone(), &args),
        Frames::Sweep => {
            let pages = usize::try_from(input.pages()).map_err(|e| e.to_string())?;
            run_sweep(&input, sweep::default_sizes(pages), &args)
        }
    }
}

/// Every policy asked for at one pool size: a summary each, and for a trace
/// the run file and step-through viewer unless --no-viewer.
fn run_policies(input: &Input, frames: usize, args: &Cli) -> Result<(), String> {
    check_frames(frames, input)?;
    let viewer = input.has_viewer(args);
    if let (Input::Trace(trace), true) = (input, viewer) {
        check_viewer_size(trace, frames, args.policy.0.len())?;
    }
    let name = input.name().to_string();
    let policies = &args.policy.0;
    let compare = policies.len() > 1;

    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let folder = if compare {
        "compare".to_string()
    } else {
        policies[0].file_name()
    };
    let run_dir = new_run_dir(&args.out.join(&name).join(folder), &ts)?;

    let mut sims = Vec::with_capacity(policies.len());
    for &policy in policies {
        let cfg = SimConfig {
            frames,
            policy,
            storage: args.storage,
            read_ahead: args.read_ahead,
        };
        let mut sim = Simulator::new(input.trace(args), cfg);
        if viewer {
            sim.record_events();
        }
        let dir = if compare {
            run_dir.join(policy.file_name())
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

    if let (Input::Trace(trace), true) = (input, viewer) {
        let run_json =
            serde_json::to_string(&RunFile::new(trace, &sims)).map_err(|e| e.to_string())?;
        write(&run_dir.join("run.json"), &format!("{run_json}\n"))?;
        write(&run_dir.join("viewer.html"), &viewer_html(&run_json))?;
    }
    say(format_args!("Run written to {}", run_dir.display()));
    Ok(())
}

/// Every policy asked for at every size in `sizes` (increasing): sweep.json
/// and the sweep page.
fn run_sweep(input: &Input, sizes: Vec<usize>, args: &Cli) -> Result<(), String> {
    check_frames(*sizes.last().expect("at least one size"), input)?;
    let started = Instant::now();
    let trace = input.trace(args);
    let lru = LruCurve::new(&trace);
    let jobs: Vec<(PolicyKind, usize)> = args
        .policy
        .0
        .iter()
        .flat_map(|&p| sizes.iter().map(move |&f| (p, f)))
        .collect();
    let points = in_parallel(&jobs, |&(policy, frames)| {
        sweep::point(&trace, &lru, policy, frames, args.read_ahead)
    });
    let name = input.name();
    let (page_size, sqlite_cache) = match input {
        Input::Trace(t) => (Some(t.page_size), &t.sqlite_cache[..]),
        Input::Workload(_) => (None, &[][..]),
    };
    let file = SweepFile {
        format: sweep::FORMAT,
        version: sweep::VERSION,
        name,
        input: input.kind(),
        pages: input.pages() as usize,
        page_size,
        queries: trace.num_queries(),
        accesses: trace.accesses().len(),
        read_ahead: args.read_ahead,
        command: command_line(),
        policies: args.policy.0.iter().map(|p| p.id()).collect(),
        sizes,
        points,
        sqlite_cache,
    };
    let json = serde_json::to_string(&file).map_err(|e| e.to_string())?;
    let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let dir = new_run_dir(&args.out.join(name).join("sweep"), &ts)?;
    write(&dir.join("sweep.json"), &format!("{json}\n"))?;
    write(&dir.join("sweep.html"), &sweep_html(&json))?;
    say(format_args!(
        "{name} | sweep | {} policies x {} sizes | {:.1} s",
        file.policies.len(),
        file.sizes.len(),
        started.elapsed().as_secs_f64()
    ));
    say(format_args!("Sweep written to {}", dir.display()));
    Ok(())
}

/// Run `job` on every item, spread over the machine's cores. Results keep
/// the items' order, so the output never depends on the number of cores.
fn in_parallel<T: Sync, R: Send>(items: &[T], job: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<R>> = items.iter().map(|_| None).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..cores.min(items.len()))
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        done.push((i, job(item)));
                    }
                    done
                })
            })
            .collect();
        for worker in workers {
            for (i, result) in worker.join().expect("a sweep worker panicked") {
                results[i] = Some(result);
            }
        }
    });
    results
        .into_iter()
        .map(|r| r.expect("every item ran"))
        .collect()
}

/// How this run was started, with the program as `sim`, to make it again.
fn command_line() -> String {
    let args: Vec<String> = std::env::args().skip(1).collect();
    format!("sim {}", args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parallel_results_keep_the_order_of_their_items() {
        // Later items finish first, so arrival order would be reversed.
        let items: Vec<u64> = (0..24).collect();
        let doubled = in_parallel(&items, |&i| {
            std::thread::sleep(std::time::Duration::from_millis(24 - i));
            i * 2
        });
        assert_eq!(doubled, items.iter().map(|i| i * 2).collect::<Vec<_>>());
    }

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
