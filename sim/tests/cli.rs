//! CLI input validation: every input that crashed v0.1 must now fail cleanly
//! with a message, and a normal run must write snapshots + summary.json.

use std::path::PathBuf;
use std::process::{Command, Output};

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
    assert_rejected(&["--recluster-every", "0"], "--recluster-every");
    assert_rejected(&["--frames", "0"], "must be at least 1");
    assert_rejected(&["--ticks", "0"], "--ticks");
    assert_rejected(&["--policy", "random"], "unknown policy");
    assert_rejected(&["--co-access-decay", "0"], "must be in (0, 1]");
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
fn snapshot_every_zero_writes_summary_only() {
    let dir = out_dir("summary_only");
    let out = sim(&[
        "--ticks",
        "50",
        "--snapshot-every",
        "0",
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = std::fs::read_dir(dir.join("default/clock/recluster"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(run.join("summary.json").exists());
    assert_eq!(std::fs::read_dir(run.join("snapshots")).unwrap().count(), 0);
}

#[test]
fn normal_run_writes_snapshots() {
    let dir = out_dir("normal_run");
    let out = sim(&[
        "--ticks",
        "100",
        "--policy",
        "2q",
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = std::fs::read_dir(dir.join("default/2q/recluster"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_dir(run.join("snapshots")).unwrap().count(),
        10
    );
    let summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["policy"], "2q");
    assert_eq!(summary["queries"], 100);
}
