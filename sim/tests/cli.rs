//! CLI input validation: every input that crashed v0.1 must now fail cleanly
//! with a message, and a normal run must write summary.json.
//! Trace runs also write run.json and viewer.html with the same data built in.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn sim(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sim"))
        .args(args)
        .output()
        .expect("run sim binary")
}

fn out_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn assert_rejected(args: &[&str], expect: &str) {
    let out = sim(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{args:?} should fail");
    assert!(stderr.contains(expect), "{args:?}: stderr was {stderr}");
    assert!(!stderr.contains("panicked"), "{args:?} panicked: {stderr}");
}

#[test]
fn rejects_inputs_that_used_to_panic() {
    assert_rejected(&["--frames", "0"], "must be at least 1");
    assert_rejected(&["--ticks", "0"], "--ticks");
    assert_rejected(&["--policy", "random"], "unknown policy");
    assert_rejected(&["--policy", "lru,random"], "unknown policy");
    assert_rejected(&["--policy", "lru,LRU"], "listed twice");
    assert_rejected(&["--policy", "pg,pg:ring=on"], "listed twice");
    assert_rejected(&["--policy", "innodb:old=99"], "from 5 to 95");
    assert_rejected(&["--policy", "lru:ring=off"], "lru has no settings");
    assert_rejected(&["--policy", "innodb:old=../x"], "whole percentage");
    // Flags retired with reclustering and the old visualiser are rejected,
    // not silently ignored.
    for gone in [
        "--recluster",
        "--recluster-every",
        "--co-access-decay",
        "--snapshot-every",
    ] {
        assert_rejected(&[gone, "1"], "unexpected argument");
    }
    assert_rejected(&["--frames", "5000"], "exceeds the workload's total pages");
}

#[test]
fn rejects_invalid_workload_file() {
    let dir = out_dir("bad_workload");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.toml");
    // Four tables, but a query references table 5 (v0.1 crashed on this).
    std::fs::write(
        &path,
        "name = \"bad\"\nnum_tables = 4\npages_per_table = 50\n\
         [[phase]]\n[[phase.query]]\ntables = [0, 5]\nweight = 1.0\n",
    )
    .unwrap();
    assert_rejected(
        &["--workload", path.to_str().unwrap()],
        "table 5 out of range",
    );
}

#[test]
fn normal_run_writes_a_summary() {
    let dir = out_dir("normal_run");
    let out = sim(&[
        "--ticks",
        "100",
        "--policy",
        "innodb",
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = only_run(&dir.join("default/innodb"));
    assert_eq!(
        std::fs::read_dir(&run).unwrap().count(),
        1,
        "summary.json only"
    );
    let summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["policy"], "innodb");
    assert_eq!(summary["queries"], 100);
}

fn shop_trace() -> String {
    format!("{}/../traces/shop.trace.json", env!("CARGO_MANIFEST_DIR"))
}

/// The only timestamped run folder under `dir`.
fn only_run(dir: &Path) -> PathBuf {
    let mut runs: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(runs.len(), 1, "{}", dir.display());
    runs.pop().unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The run file built into viewer.html, read the way a browser would: the
/// script element ends at the first `</script>`.
fn embedded_run(html: &Path) -> Value {
    let html = std::fs::read_to_string(html).unwrap();
    let open = r#"<script id="run" type="application/json">"#;
    let start = html.find(open).unwrap() + open.len();
    let end = start + html[start..].find("</script>").unwrap();
    serde_json::from_str(&html[start..end]).unwrap()
}

/// A one-query trace whose single page row holds `cell`.
fn tiny_trace(dir: &Path, reads: usize, cell: &str) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let io: Vec<String> = (0..reads)
        .map(|i| format!(r#"["r",{}]"#, i % 100 + 1))
        .collect();
    let json = format!(
        r#"{{"format":"bpsim-trace","version":2,"scenario":"tiny","sqlite_version":"3",
           "page_size":4096,"page_count":100,"objects":[],"notes":[],
           "pages":[{{"page":2,"object":"t","kind":"leaf"}}],
           "rows":{{"2":{{"object":"t","columns":["name"],"rows":[[{cell}]]}}}},
           "queries":[{{"title":"q","sql":"SELECT 1;","plan":[],"scans":[],"io":[{}]}}]}}"#,
        io.join(",")
    );
    let path = dir.join("tiny.trace.json");
    std::fs::write(&path, json).unwrap();
    path.to_str().unwrap().to_string()
}

#[test]
fn replays_a_captured_trace() {
    let dir = out_dir("replay");
    let trace = shop_trace();
    let out = sim(&[
        "--trace",
        &trace,
        "--policy",
        "lru",
        "--frames",
        "8",
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = only_run(&dir.join("shop/lru"));
    let summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["trace"], "shop");
    // Read-ahead is on by default: the report's scan of orders reads pages
    // ahead, so 9 of its misses become hits (15 with --read-ahead off).
    assert_eq!(summary["hits"], 24);
    assert_eq!(
        (&summary["pages_read_ahead"], &summary["read_ahead_hits"]),
        (&Value::from(9), &Value::from(9))
    );
    assert!(summary.get("seed").is_none() && summary.get("workload").is_none());
    let run_file = read_json(&run.join("run.json"));
    assert_eq!(run_file["runs"].as_array().unwrap().len(), 1);
    assert_eq!(embedded_run(&run.join("viewer.html")), run_file);
}

#[test]
fn comparing_policies_writes_one_run_file_that_adds_up() {
    let dir = out_dir("compare");
    let trace = shop_trace();
    let args = ["--trace", &trace, "--policy", "all", "--frames", "8"];
    let out = sim(&[&args[..], &["--out", dir.to_str().unwrap()]].concat());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = only_run(&dir.join("shop/compare"));
    let run_file = read_json(&run.join("run.json"));
    assert_eq!(
        (&run_file["format"], &run_file["version"]),
        (&Value::from("bpsim-run"), &Value::from(6))
    );
    assert_eq!(run_file["storage"], "ssd");
    assert_eq!(run_file["frames"], 8);
    assert_eq!(
        run_file["queries"][4]["accesses"],
        serde_json::json!([14, 31])
    );

    let runs = run_file["runs"].as_array().unwrap();
    let names: Vec<_> = runs.iter().map(|r| r["policy"].as_str().unwrap()).collect();
    assert_eq!(names, ["fifo", "lru", "clock", "pg", "innodb", "opt"]);
    for r in runs {
        let policy = r["policy"].as_str().unwrap();
        let summary = read_json(&run.join(policy).join("summary.json"));
        assert_eq!(summary["hits"], r["summary"]["hits"], "{policy}");

        let events = r["events"].as_array().unwrap();
        assert_eq!(events.len(), 45, "{policy}");
        let hits = events.iter().filter(|e| e["kind"] == "hit").count();
        assert_eq!(hits as u64, summary["hits"].as_u64().unwrap(), "{policy}");
        let count = |key: &str| -> u64 {
            events
                .iter()
                .map(|e| e[key].as_array().map_or(0, |a| a.len() as u64))
                .sum()
        };
        let ahead_hits = events
            .iter()
            .filter(|e| e["read_ahead_hit"] == true)
            .count() as u64;
        assert_eq!(
            count("ahead"),
            summary["pages_read_ahead"].as_u64().unwrap(),
            "{policy}"
        );
        assert_eq!(
            ahead_hits,
            summary["read_ahead_hits"].as_u64().unwrap(),
            "{policy}"
        );
        if policy == "opt" {
            assert_eq!(count("ahead"), 0, "Optimal never reads ahead");
        }
        let ms = priced(events, &run_file["profiles"]["ssd"]);
        let total = summary["total_ms"].as_f64().unwrap();
        assert!(
            (ms - total).abs() < 1e-9 * total,
            "{policy}: {ms} vs {total}"
        );
        assert!(
            events
                .iter()
                .all(|e| e["frames"].as_array().unwrap().len() == 8)
        );
    }
    assert_eq!(embedded_run(&run.join("viewer.html")), run_file);
}

/// What the viewer does: price each event from what it records, with a
/// profile from the run file. Written from the model's description, not from
/// the engine's code: a read request is a fixed part plus one page; reading
/// further on from the last page read pays the near overhead plus the
/// skipped pages, never more than a jump.
fn priced(events: &[Value], profile: &Value) -> f64 {
    let ms = |key: &str| profile[key].as_f64().unwrap();
    let (jump, near, page) = (ms("read_request_ms"), ms("near_request_ms"), ms("page_ms"));
    events
        .iter()
        .map(|e| {
            let read = match e["read"].as_str() {
                None if e["kind"] == "hit" => ms("hit_ms"),
                // A page SQLite wrote without reading: no disk read.
                None => 0.0,
                Some(kind) => {
                    // The missed page and the pages read ahead with it.
                    let pages = 1.0 + e["ahead"].as_array().map_or(0, |a| a.len()) as f64;
                    let fixed = match kind {
                        "next" => near.min(jump),
                        "ahead" => (near + e["skipped"].as_f64().unwrap() * page).min(jump),
                        "jump" => jump,
                        other => panic!("unknown read kind {other}"),
                    };
                    fixed + pages * page
                }
            };
            let read_ahead_evictions = e["ahead_evicted"].as_array().cloned().unwrap_or_default();
            let writebacks = std::iter::once(&e["evicted"])
                .chain(read_ahead_evictions.iter())
                .filter(|v| v["dirty"] == true)
                .count() as f64;
            read + writebacks * (ms("write_request_ms") + page)
        })
        .sum()
}

/// One run file is enough for either storage: pricing an SSD run's events
/// with the HDD profile gives exactly what an HDD run reports.
#[test]
fn the_viewer_can_price_one_run_for_every_storage() {
    let dir = out_dir("storage");
    let trace = format!(
        "{}/../traces/checkout.trace.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let base = [
        "--trace",
        trace.as_str(),
        "--policy",
        "lru,pg",
        "--frames",
        "6",
    ];
    let mut runs = Vec::new();
    for storage in ["ssd", "hdd"] {
        let out_root = dir.join(storage);
        let extra = ["--storage", storage, "--out", out_root.to_str().unwrap()];
        let out = sim(&[&base[..], &extra[..]].concat());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        runs.push(read_json(
            &only_run(&out_root.join("checkout/compare")).join("run.json"),
        ));
    }
    let [ssd, hdd] = [&runs[0], &runs[1]];
    assert_eq!(ssd["profiles"], hdd["profiles"]);
    for i in 0..2 {
        let events = ssd["runs"][i]["events"].as_array().unwrap();
        assert_eq!(events, hdd["runs"][i]["events"].as_array().unwrap());
        let hdd_total = hdd["runs"][i]["summary"]["total_ms"].as_f64().unwrap();
        let priced = priced(events, &ssd["profiles"]["hdd"]);
        assert!(
            (priced - hdd_total).abs() < 1e-9 * hdd_total,
            "{priced} vs {hdd_total}"
        );
    }
    assert_rejected(
        &["--trace", &trace, "--storage", "tape"],
        "expected ssd or hdd",
    );
    assert_rejected(
        &["--trace", &trace, "--read-ahead", "lots"],
        "expected auto, off or a page count",
    );
}

#[test]
fn database_text_cannot_break_out_of_the_viewer_page() {
    let dir = out_dir("escape");
    let trace = tiny_trace(&dir, 3, r#""</script><!-- <b>""#);
    let out = sim(&[
        "--trace",
        &trace,
        "--frames",
        "2",
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = only_run(&dir.join("tiny/clock"));
    let embedded = embedded_run(&run.join("viewer.html"));
    assert_eq!(embedded["rows"]["2"]["rows"][0][0], "</script><!-- <b>");
    assert_eq!(embedded, read_json(&run.join("run.json")));
}

#[test]
fn comparing_policies_on_a_workload_writes_summaries_only() {
    let dir = out_dir("compare_workload");
    let out = sim(&[
        "--ticks",
        "20",
        "--policy",
        "lru,opt",
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = only_run(&dir.join("default/compare"));
    for policy in ["lru", "opt"] {
        assert_eq!(
            read_json(&run.join(policy).join("summary.json"))["policy"],
            policy
        );
    }
    assert!(!run.join("run.json").exists() && !run.join("viewer.html").exists());
}

#[test]
fn rejects_bad_trace_arguments() {
    let trace = shop_trace();
    assert_rejected(
        &["--trace", &trace, "--workload", "x.toml"],
        "cannot be used with",
    );
    assert_rejected(
        &["--trace", &trace, "--frames", "500"],
        "exceeds the trace's total pages",
    );
    // 600 accesses x 100 frames x 6 policies is past the run file limit.
    let big = tiny_trace(&out_dir("big_trace"), 600, "1");
    assert_rejected(
        &["--trace", &big, "--frames", "100", "--policy", "all"],
        "use fewer --frames",
    );
    // Past the limit with one frame and one policy: fewer frames cannot help.
    let huge = tiny_trace(&out_dir("huge_trace"), 250_001, "1");
    assert_rejected(
        &["--trace", &huge, "--frames", "1", "--policy", "lru"],
        "even with one frame and one policy",
    );
}

/// `sim ... | head -1` closes the pipe after one line; the run still finishes
/// and writes its files instead of panicking.
#[test]
fn a_reader_that_stops_early_does_not_break_the_run() {
    let dir = out_dir("closed-pipe");
    let trace = format!("{}/../traces/shop.trace.json", env!("CARGO_MANIFEST_DIR"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_sim"))
        .args(["--trace", &trace, "--policy", "all", "--frames", "8"])
        .args(["--out", dir.to_str().unwrap()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run sim binary");
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        only_run(&dir.join("shop/compare"))
            .join("viewer.html")
            .is_file()
    );
}

#[test]
fn runs_started_in_the_same_second_get_their_own_folders() {
    let out = out_dir("same_second");
    let trace = shop_trace();
    let args = ["--trace", &trace, "--policy", "lru", "--frames", "8"];
    for _ in 0..3 {
        let done = sim(&[&args[..], &["--out", out.to_str().unwrap()]].concat());
        assert!(done.status.success());
    }
    let parent = out.join("shop").join("lru");
    let runs: Vec<_> = std::fs::read_dir(&parent).unwrap().collect();
    assert_eq!(runs.len(), 3, "a later run overwrote an earlier one");
    for run in runs {
        assert!(run.unwrap().path().join("run.json").is_file());
    }
}

/// The `VERSION = n;` a page declares.
fn page_version(page: &str) -> Vec<u32> {
    page.lines()
        .filter_map(|l| l.trim().strip_prefix("VERSION = "))
        .map(|v| v.trim_end_matches(';').parse().expect("a number"))
        .collect()
}

/// The viewer and the sweep page refuse files of another version, so their
/// VERSION must move with the engine's.
#[test]
fn the_pages_read_the_file_versions_the_engine_writes() {
    let viewer = page_version(include_str!("../../viewer/index.html"));
    assert_eq!(viewer, [bpsim::run::VERSION], "viewer/index.html");
    let sweep = page_version(include_str!("../../viewer/sweep.html"));
    assert_eq!(sweep, [bpsim::sweep::VERSION], "viewer/sweep.html");
}

#[test]
fn a_gzipped_trace_replays_like_the_plain_one() {
    use std::io::Write;
    let out = out_dir("gzip");
    let plain = shop_trace();
    let zipped = out.join("shop.trace.json.gz");
    std::fs::create_dir_all(&out).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&std::fs::read(&plain).unwrap()).unwrap();
    std::fs::write(&zipped, gz.finish().unwrap()).unwrap();
    let run = |trace: &str| {
        let done = sim(&[
            "--trace",
            trace,
            "--policy",
            "lru",
            "--frames",
            "8",
            "--out",
            out.to_str().unwrap(),
        ]);
        assert!(
            done.status.success(),
            "{}",
            String::from_utf8_lossy(&done.stderr)
        );
        String::from_utf8(done.stdout)
            .unwrap()
            .lines()
            .find(|l| l.contains("hit rate"))
            .unwrap()
            .to_string()
    };
    assert_eq!(run(&plain), run(zipped.to_str().unwrap()));
    let broken = out.join("broken.trace.json.gz");
    std::fs::write(&broken, b"not gzip at all").unwrap();
    let done = sim(&[
        "--trace",
        broken.to_str().unwrap(),
        "--policy",
        "lru",
        "--frames",
        "8",
    ]);
    assert!(!done.status.success());
    assert!(String::from_utf8_lossy(&done.stderr).contains("broken.trace.json.gz"));
}

/// The folder a run or sweep was written to, from its last line of output.
fn written_to(out: &Output) -> PathBuf {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout.lines().last().unwrap();
    PathBuf::from(last.split(" written to ").nth(1).unwrap())
}

fn embedded_sweep(html: &Path) -> Value {
    let html = std::fs::read_to_string(html).unwrap();
    let open = r#"<script id="sweep" type="application/json">"#;
    let start = html.find(open).unwrap() + open.len();
    let end = start + html[start..].find("</script>").unwrap();
    serde_json::from_str(&html[start..end]).unwrap()
}

/// Every sweep point equals the single runs it stands for: read-ahead off
/// for disk reads, the default read-ahead for time on each drive. Checkout
/// writes a page it never read, which must not count as a disk read.
#[test]
fn a_sweep_point_matches_the_single_runs_it_stands_for() {
    let dir = out_dir("sweep");
    let trace = format!(
        "{}/../traces/checkout.trace.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let out = dir.to_str().unwrap();
    let swept = written_to(&sim(&[
        "--trace",
        &trace,
        "--policy",
        "lru,innodb",
        "--frames",
        "24,8",
        "--out",
        out,
    ]));
    let sweep = read_json(&swept.join("sweep.json"));
    assert_eq!(embedded_sweep(&swept.join("sweep.html")), sweep);
    assert_eq!(sweep["sizes"], serde_json::json!([8, 24]));
    assert_eq!(sweep["pages"], 32);
    let points = sweep["points"].as_array().unwrap();
    let order: Vec<(&str, u64)> = points
        .iter()
        .map(|p| (p["policy"].as_str().unwrap(), p["frames"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        order,
        [("lru", 8), ("lru", 24), ("innodb", 8), ("innodb", 24)]
    );
    let mut fresh_seen = 0;
    for p in points {
        let (policy, frames) = (p["policy"].as_str().unwrap(), p["frames"].to_string());
        let single = |extra: &[&str]| {
            let mut args = vec![
                "--trace",
                &trace,
                "--policy",
                policy,
                "--frames",
                &frames,
                "--no-viewer",
                "--out",
                out,
            ];
            args.extend(extra);
            read_json(&written_to(&sim(&args)).join("summary.json"))
        };
        let off = single(&["--read-ahead", "off"]);
        assert_eq!(p["hits"], off["hits"], "{policy} {frames}");
        assert_eq!(p["misses"], off["misses"]);
        let fresh = off["fresh_pages"].as_u64().unwrap();
        fresh_seen += fresh;
        assert_eq!(p["disk_reads"], off["misses"].as_u64().unwrap() - fresh);
        let ssd = single(&[]);
        assert_eq!(p["read_requests"], ssd["read_requests"]);
        let read = ssd["misses"].as_u64().unwrap() - ssd["fresh_pages"].as_u64().unwrap()
            + ssd["pages_read_ahead"].as_u64().unwrap();
        assert_eq!(p["pages_read"], read);
        assert_eq!(p["ssd_ms_per_query"], ssd["mean_ms_per_query"]);
        let hdd = single(&["--storage", "hdd"]);
        assert_eq!(p["hdd_ms_per_query"], hdd["mean_ms_per_query"]);
        // The smallest pool where LRU reads no more: one frame less reads more.
        let lru_reads = |frames: u64| {
            let f = frames.to_string();
            let args = [
                "--trace",
                &trace,
                "--policy",
                "lru",
                "--frames",
                &f,
                "--read-ahead",
                "off",
                "--no-viewer",
                "--out",
                out,
            ];
            let s = read_json(&written_to(&sim(&args)).join("summary.json"));
            s["misses"].as_u64().unwrap() - s["fresh_pages"].as_u64().unwrap()
        };
        let need = p["lru_frames"].as_u64().unwrap();
        let reads = p["disk_reads"].as_u64().unwrap();
        assert!(lru_reads(need) <= reads, "{policy} {frames}: LRU at {need}");
        assert!(
            need == 1 || lru_reads(need - 1) > reads,
            "{policy} {frames}: {need} is not the least"
        );
    }
    assert!(
        fresh_seen > 0,
        "the trace must have pages written without a read"
    );
}

#[test]
fn policies_with_settings_run_side_by_side_under_their_ids() {
    let dir = out_dir("settings");
    let trace = shop_trace();
    let policies = "innodb,innodb:old=20,innodb:promote=any,pg:ring=off";
    let args = [
        "--trace",
        &trace,
        "--policy",
        policies,
        "--frames",
        "8",
        "--read-ahead",
        "off",
    ];
    let out = sim(&[&args[..], &["--out", dir.to_str().unwrap()]].concat());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = only_run(&dir.join("shop/compare"));
    for folder in [
        "innodb",
        "innodb-old-20",
        "innodb-promote-any",
        "pg-ring-off",
    ] {
        assert!(run.join(folder).join("summary.json").is_file(), "{folder}");
    }
    let run_file = read_json(&run.join("run.json"));
    let ids: Vec<_> = run_file["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["policy"].clone(), r["summary"]["policy"].clone()))
        .collect();
    let expect = [
        "innodb",
        "innodb:old=20",
        "innodb:promote=any",
        "pg:ring=off",
    ];
    assert_eq!(ids, expect.map(|p| (Value::from(p), Value::from(p))));
    // The old share changes what is cached; the plain policy is unchanged.
    let runs = run_file["runs"].as_array().unwrap();
    assert_eq!(
        runs[0]["summary"]["hits"],
        Value::from(20),
        "the design number"
    );
    assert_ne!(runs[0]["summary"]["hits"], runs[1]["summary"]["hits"]);

    let one = out_dir("settings_one");
    let args = [
        "--trace",
        &trace,
        "--policy",
        "innodb:old=20",
        "--frames",
        "8",
    ];
    let out = sim(&[&args[..], &["--out", one.to_str().unwrap()]].concat());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    only_run(&one.join("shop/innodb-old-20"));

    let sw = out_dir("settings_sweep");
    let args = [
        "--trace",
        &trace,
        "--policy",
        "lru,pg:ring=off",
        "--frames",
        "8,16",
    ];
    let out = sim(&[&args[..], &["--out", sw.to_str().unwrap()]].concat());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sweep = read_json(&written_to(&out).join("sweep.json"));
    assert_eq!(sweep["policies"], serde_json::json!(["lru", "pg:ring=off"]));
    assert!(
        sweep["points"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["policy"] == "pg:ring=off")
    );
}

#[test]
fn a_default_sweep_runs_every_policy_from_8_frames_to_the_whole_database() {
    let dir = out_dir("sweep_default");
    let swept = written_to(&sim(&[
        "--trace",
        &shop_trace(),
        "--policy",
        "all",
        "--frames",
        "sweep",
        "--out",
        dir.to_str().unwrap(),
    ]));
    let sweep = read_json(&swept.join("sweep.json"));
    assert_eq!(
        sweep["sizes"],
        serde_json::json!([8, 12, 16, 24, 32, 48, 64, 80])
    );
    assert_eq!(sweep["points"].as_array().unwrap().len(), 6 * 8);
    assert_eq!(sweep["format"], "bpsim-sweep");
    assert!(
        sweep["command"]
            .as_str()
            .unwrap()
            .starts_with("sim --trace ")
    );
    assert_rejected(
        &["--trace", &shop_trace(), "--frames", "8,500"],
        "exceeds the trace's total pages (80)",
    );
}

/// --no-viewer writes only summaries, so a trace past the viewer's limit runs.
#[test]
fn without_the_viewer_a_trace_of_any_length_runs() {
    let dir = out_dir("no_viewer");
    let huge = tiny_trace(&dir.join("trace"), 250_001, "1");
    let run = written_to(&sim(&[
        "--trace",
        &huge,
        "--policy",
        "lru",
        "--frames",
        "4",
        "--no-viewer",
        "--out",
        dir.to_str().unwrap(),
    ]));
    assert_eq!(read_json(&run.join("summary.json"))["accesses"], 250_001);
    assert!(!run.join("run.json").exists() && !run.join("viewer.html").exists());
}
