//! Traces captured from SQLite by `capture/capture.py` (format `bpsim-trace`,
//! version 1). Only the fields the engine replays are read here; page labels
//! and decoded rows are left for the viewer.
//!
//! The captured database file is modelled as a single "table" whose page ids
//! are SQLite's own page numbers, so seek distance is the real distance
//! between pages in the file and reclustering has nothing to reorder.
//!
//! SQLite logs a write when a statement commits, after the statement already
//! read and changed that page in memory. Replaying the write as its own request
//! would count it as an extra cache hit, or with a tiny pool as a disk read that
//! never happened. So each write is folded into the same query's latest read of
//! that page ("read, then modify", like synthetic workloads); a write to a page
//! the query never read stays a write access.
//!
//! A query whose plan has `SCAN <table>` reads that table's b-tree in order, so
//! its accesses to that table's pages are marked as a scan of a relation that
//! many pages long. With `USING [COVERING] INDEX <index>` the index is what is
//! read in order, so its pages are marked instead.

use std::collections::HashMap;

use serde::Deserialize;

use crate::sim::{SimConfig, Simulator};
use crate::trace::{Access, Trace};
use crate::workload::CostModel;

pub const FORMAT: &str = "bpsim-trace";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct CapturedTrace {
    pub format: String,
    pub version: u32,
    pub scenario: String,
    pub sqlite_version: String,
    pub page_size: u32,
    pub page_count: u32,
    /// Which table or index owns each page.
    pub pages: Vec<PageInfo>,
    pub queries: Vec<CapturedQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageInfo {
    pub page: u32,
    pub object: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CapturedQuery {
    pub title: String,
    pub sql: String,
    pub plan: Vec<String>,
    pub io: Vec<(IoOp, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum IoOp {
    #[serde(rename = "r")]
    Read,
    #[serde(rename = "w")]
    Write,
}

impl CapturedTrace {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let t: Self = serde_json::from_str(text).map_err(|e| format!("invalid trace JSON: {e}"))?;
        t.validate()?;
        Ok(t)
    }

    fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT {
            return Err(format!("expected format {FORMAT:?}, got {:?}", self.format));
        }
        if self.version != VERSION {
            return Err(format!(
                "unsupported trace version {} (this build reads version {VERSION})",
                self.version
            ));
        }
        if self.scenario.is_empty()
            || !self
                .scenario
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "scenario name must match [A-Za-z0-9_-]+, got {:?}",
                self.scenario
            ));
        }
        if self.page_count == 0 {
            return Err("page_count must be at least 1".into());
        }
        if self.queries.is_empty() {
            return Err("trace has no queries".into());
        }
        for q in &self.queries {
            if let Some(&(_, page)) = q.io.iter().find(|&&(_, p)| p == 0 || p > self.page_count) {
                return Err(format!(
                    "query {:?} touches page {page}, outside 1..={}",
                    q.title, self.page_count
                ));
            }
        }
        Ok(())
    }

    /// One query per tick, with writes folded into reads and scans marked
    /// (see the module docs).
    pub fn to_trace(&self) -> Trace {
        let owner: HashMap<u32, &str> = self
            .pages
            .iter()
            .map(|p| (p.page, p.object.as_str()))
            .collect();
        let mut size: HashMap<&str, u32> = HashMap::new();
        for p in &self.pages {
            *size.entry(p.object.as_str()).or_default() += 1;
        }
        let queries = self
            .queries
            .iter()
            .map(|q| {
                let scanned: Vec<&str> = q.plan.iter().filter_map(|l| scanned_object(l)).collect();
                let scan_of = |page: u32| {
                    let object = *owner.get(&page)?;
                    scanned.contains(&object).then(|| size[object])
                };
                fold_writes(q, scan_of)
            })
            .collect();
        // Page 0 does not exist in SQLite; reserving it keeps ids equal to page numbers.
        Trace::from_queries(1, self.page_count + 1, queries)
    }

    /// A simulator ready to replay this trace. Reclustering never applies.
    pub fn simulator(&self, cfg: SimConfig) -> Simulator {
        let cfg = SimConfig {
            recluster_every: None,
            ..cfg
        };
        Simulator::new(self.to_trace(), vec![0], CostModel::default(), cfg)
    }
}

/// The table or index a `SCAN` plan line reads in order, if any.
fn scanned_object(plan_line: &str) -> Option<&str> {
    let rest = plan_line.strip_prefix("SCAN ")?;
    let mut words = rest.split_whitespace();
    let table = words.next()?;
    let index = rest
        .split_once("USING COVERING INDEX ")
        .or_else(|| rest.split_once("USING INDEX "))
        .and_then(|(_, after)| after.split_whitespace().next());
    Some(index.unwrap_or(table))
}

fn fold_writes(q: &CapturedQuery, scan_of: impl Fn(u32) -> Option<u32>) -> Vec<Access> {
    let mut accesses: Vec<Access> = Vec::with_capacity(q.io.len());
    for &(op, page) in &q.io {
        let earlier = accesses.iter_mut().rev().find(|a| a.page == page);
        match (op, earlier) {
            (IoOp::Write, Some(read)) => read.write = true,
            (op, _) => accesses.push(Access {
                page,
                write: op == IoOp::Write,
                scan: scan_of(page),
            }),
        }
    }
    accesses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace_json(format: &str, version: u32, io: &str) -> String {
        format!(
            r#"{{"format":"{format}","version":{version},"scenario":"tiny","sqlite_version":"3.54.1",
               "page_size":4096,"page_count":3,"rows":{{}},"notes":[],
               "pages":[{{"page":1,"object":"sqlite_schema","kind":"leaf"}},
                        {{"page":2,"object":"t","kind":"leaf"}},
                        {{"page":3,"object":"t","kind":"leaf"}}],
               "queries":[{{"title":"q","sql":"SELECT 1;","plan":["SCAN t"],"io":{io}}}]}}"#
        )
    }

    fn accesses(io: &str) -> Vec<(u32, bool)> {
        let t = CapturedTrace::from_json(&trace_json(FORMAT, 1, io)).unwrap();
        let trace = t.to_trace();
        assert_eq!(trace.num_queries(), 1);
        assert_eq!(trace.table_of(3), 0);
        trace.accesses().iter().map(|a| (a.page, a.write)).collect()
    }

    #[test]
    fn scan_marks_only_the_scanned_objects_pages() {
        let t = CapturedTrace::from_json(&trace_json(FORMAT, 1, r#"[["r",1],["r",2],["r",3]]"#))
            .unwrap();
        let scans: Vec<_> = t.to_trace().accesses().iter().map(|a| a.scan).collect();
        assert_eq!(scans, [None, Some(2), Some(2)]);
    }

    #[test]
    fn plan_lines_name_the_object_read_in_order() {
        assert_eq!(scanned_object("SCAN orders"), Some("orders"));
        assert_eq!(
            scanned_object("SCAN users USING COVERING INDEX users_email"),
            Some("users_email")
        );
        assert_eq!(
            scanned_object("SCAN users USING INDEX users_email"),
            Some("users_email")
        );
        assert_eq!(
            scanned_object("SEARCH users USING INTEGER PRIMARY KEY (rowid=?)"),
            None
        );
    }

    #[test]
    fn writes_fold_into_the_query_read_of_the_same_page() {
        assert_eq!(
            accesses(r#"[["r",1],["r",3],["w",3]]"#),
            [(1, false), (3, true)]
        );
    }

    #[test]
    fn a_write_without_a_read_stays_a_write_access() {
        assert_eq!(accesses(r#"[["r",1],["w",2]]"#), [(1, false), (2, true)]);
    }

    #[test]
    fn a_folded_write_causes_no_extra_disk_read_in_a_tiny_pool() {
        // With two frames, page 1 is evicted before the commit writes it.
        let t = CapturedTrace::from_json(&trace_json(
            FORMAT,
            1,
            r#"[["r",1],["r",2],["r",3],["w",1]]"#,
        ))
        .unwrap();
        let mut sim = t.simulator(SimConfig {
            frames: 2,
            policy: crate::PolicyKind::Lru,
            ..SimConfig::default()
        });
        while sim.step().is_some() {}
        let s = sim.summary();
        assert_eq!((s.accesses, s.misses), (3, 3));
    }

    #[test]
    fn rejects_bad_traces() {
        let cases = [
            (trace_json("other", 1, "[]"), "expected format"),
            (trace_json(FORMAT, 2, "[]"), "unsupported trace version"),
            (trace_json(FORMAT, 1, r#"[["r",4]]"#), "outside 1..=3"),
            (trace_json(FORMAT, 1, r#"[["r",0]]"#), "outside 1..=3"),
            (trace_json(FORMAT, 1, r#"[["x",1]]"#), "unknown variant"),
        ];
        for (json, expect) in cases {
            let err = CapturedTrace::from_json(&json).unwrap_err();
            assert!(err.contains(expect), "{expect}: {err}");
        }
    }
}
