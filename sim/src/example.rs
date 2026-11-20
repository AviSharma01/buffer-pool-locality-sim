//! The worked example in the viewer's "How it works": a tiny database replayed
//! by the engine itself on a four-frame LRU pool, so the explainer cannot drift
//! from what the engine does. A test checks it still shows every kind of step
//! the walkthrough talks about.

use crate::readahead::ReadAhead;
use crate::run::RunFile;
use crate::{CapturedTrace, PolicyKind, SimConfig, StorageKind};

/// Pool size of the example.
pub const FRAMES: usize = 4;

/// Page 1 is the schema; `users` is pages 2-4 and `orders` pages 5-9. Two
/// lookups, a scan, and the first lookup again: the scan pushes the user's
/// pages out, which is LRU's known weakness.
const TRACE: &str = r#"{
  "format": "bpsim-trace", "version": 2, "scenario": "example",
  "sqlite_version": "built in", "page_size": 4096, "page_count": 9,
  "objects": [
    {"name": "sqlite_schema", "type": "table", "table": "sqlite_schema"},
    {"name": "users", "type": "table", "table": "users"},
    {"name": "orders", "type": "table", "table": "orders"}
  ],
  "pages": [
    {"page": 1, "object": "sqlite_schema", "kind": "leaf"},
    {"page": 2, "object": "users", "kind": "internal"},
    {"page": 3, "object": "users", "kind": "leaf"},
    {"page": 4, "object": "users", "kind": "leaf"},
    {"page": 5, "object": "orders", "kind": "internal"},
    {"page": 6, "object": "orders", "kind": "leaf"},
    {"page": 7, "object": "orders", "kind": "leaf"},
    {"page": 8, "object": "orders", "kind": "leaf"},
    {"page": 9, "object": "orders", "kind": "leaf"}
  ],
  "queries": [
    {"title": "Find a user", "sql": "SELECT * FROM users WHERE id = 7;",
     "plan": [], "scans": [], "io": [["r", 1], ["r", 2], ["r", 3]]},
    {"title": "Find another user", "sql": "SELECT * FROM users WHERE id = 9;",
     "plan": [], "scans": [], "io": [["r", 1], ["r", 2], ["r", 4]]},
    {"title": "Add up every order", "sql": "SELECT sum(total) FROM orders;",
     "plan": [], "scans": ["orders"],
     "io": [["r", 5], ["r", 6], ["r", 7], ["r", 8], ["r", 9]]},
    {"title": "Find the same user again", "sql": "SELECT * FROM users WHERE id = 7;",
     "plan": [], "scans": [], "io": [["r", 1], ["r", 2], ["r", 3]]}
  ],
  "rows": {}, "notes": []
}"#;

/// The example, replayed: a run file the viewer reads like any other.
pub fn run_json() -> String {
    let trace = CapturedTrace::from_json(TRACE).expect("the built-in example is a valid trace");
    let mut sim = trace.simulator(SimConfig {
        frames: FRAMES,
        policy: PolicyKind::Lru,
        storage: StorageKind::Ssd,
        read_ahead: ReadAhead::Auto,
    });
    sim.record_events();
    while sim.step().is_some() {}
    let sims = [sim];
    serde_json::to_string(&RunFile::new(&trace, &sims)).expect("serialisable")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The walkthrough explains a miss, a read-ahead, a hit on a page read
    /// ahead, an ordinary hit and an eviction; each must still happen.
    #[test]
    fn the_example_shows_every_kind_of_step() {
        let run: serde_json::Value = serde_json::from_str(&run_json()).unwrap();
        let events = run["runs"][0]["events"].as_array().unwrap();
        let has = |f: &dyn Fn(&serde_json::Value) -> bool| events.iter().any(f);
        assert!(has(&|e| e["kind"] == "miss"), "a miss");
        assert!(
            has(&|e| e["ahead"].as_array().is_some_and(|a| !a.is_empty())),
            "a read-ahead"
        );
        assert!(
            has(&|e| e["read_ahead_hit"] == true),
            "a hit on a page read ahead"
        );
        assert!(
            has(&|e| e["kind"] == "hit" && e["read_ahead_hit"] != true),
            "an ordinary hit"
        );
        assert!(has(&|e| !e["evicted"].is_null()), "an eviction");
    }
}
