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
        r#"{{"format":"bpsim-trace","version":1,"scenario":"tiny","sqlite_version":"3",
           "page_size":4096,"page_count":100,"objects":[],"notes":[],
           "pages":[{{"page":2,"object":"t","kind":"leaf"}}],
           "rows":{{"2":{{"object":"t","columns":["name"],"rows":[[{cell}]]}}}},
           "queries":[{{"title":"q","sql":"SELECT 1;","plan":[],"io":[{}]}}]}}"#,
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
    assert_eq!(summary["hits"], 15);
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
        (&Value::from("bpsim-run"), &Value::from(3))
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

/// What the viewer does: price each event from its kind with a profile from
/// the run file.
fn priced(events: &[Value], profile: &Value) -> f64 {
    let ms = |key: &str| profile[key].as_f64().unwrap();
    events
        .iter()
        .map(|e| {
            let read = match e["read"].as_str() {
                None if e["kind"] == "hit" => ms("hit_ms"),
                // A page SQLite wrote without reading: no disk read.
                None => 0.0,
                Some("sequential") => ms("sequential_read_ms"),
                Some("random") => ms("random_read_ms"),
                Some(other) => panic!("unknown read kind {other}"),
            };
            let writeback = if e["evicted"]["dirty"] == true {
                ms("write_ms")
            } else {
                0.0
            };
            read + writeback
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
        "frame entries",
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
