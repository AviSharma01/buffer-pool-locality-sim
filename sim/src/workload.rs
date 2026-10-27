//! Workload specification, loaded from TOML (see `sim/workloads/`).
//!
//! A workload is either *synthetic* (one or more `[[phase]]` blocks describing
//! a query mix, sampled with a seeded RNG) or *explicit* (a fixed reference
//! string in `explicit_pages`, one page per query).

use serde::Deserialize;

use crate::TableId;

/// The workload used when no `--workload` file is given. Reproduces the
/// original v0.1 simulator exactly.
pub const DEFAULT_WORKLOAD_TOML: &str = include_str!("../workloads/default.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    /// Short identifier, used in output paths. `[A-Za-z0-9_-]+`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub num_tables: u16,
    pub pages_per_table: u32,
    /// Probability that an access writes (dirties) the page.
    #[serde(default = "defaults::write_prob")]
    pub write_prob: f64,
    #[serde(default)]
    pub cost: CostModel,
    #[serde(default, rename = "phase")]
    pub phases: Vec<Phase>,
    /// Fixed reference string of page ids. Mutually exclusive with phases.
    #[serde(default)]
    pub explicit_pages: Option<Vec<u32>>,
}

/// A stretch of the run with one query mix. A phase is active from its
/// `start_tick` until the next phase starts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    #[serde(default = "defaults::start_tick")]
    pub start_tick: u64,
    /// Probability a query only touches each table's hot range.
    #[serde(default = "defaults::hot_prob")]
    pub hot_prob: f64,
    /// Fraction of each table (from page 0) that is hot.
    #[serde(default = "defaults::hot_fraction")]
    pub hot_fraction: f64,
    /// Inclusive [min, max] pages read from each table a query touches.
    #[serde(default = "defaults::pages_per_query")]
    pub pages_per_query: [u32; 2],
    /// Probability a query also runs a sequential scan of a random table.
    #[serde(default)]
    pub scan_prob: f64,
    /// Pages per sequential scan.
    #[serde(default = "defaults::scan_len")]
    pub scan_len: u32,
    #[serde(rename = "query")]
    pub queries: Vec<QueryTemplate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryTemplate {
    /// Tables this query joins.
    pub tables: Vec<TableId>,
    /// Relative frequency (need not sum to 1).
    pub weight: f64,
}

/// I/O cost model. Units are arbitrary; only ratios matter.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CostModel {
    /// Cost of a buffer pool hit.
    pub hit: f64,
    /// Fixed cost of reading a page from disk on a miss.
    pub miss: f64,
    /// Seek cost of moving the disk head across the whole disk; scaled
    /// linearly by distance from the previously fetched page.
    pub seek_max: f64,
    /// Extra cost when the evicted page is dirty and must be written back.
    pub dirty_writeback: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            hit: 1.0,
            miss: 10.0,
            seek_max: 30.0,
            dirty_writeback: 20.0,
        }
    }
}

mod defaults {
    pub fn write_prob() -> f64 {
        0.05
    }
    pub fn start_tick() -> u64 {
        1
    }
    pub fn hot_prob() -> f64 {
        0.7
    }
    pub fn hot_fraction() -> f64 {
        0.1
    }
    pub fn pages_per_query() -> [u32; 2] {
        [2, 5]
    }
    pub fn scan_len() -> u32 {
        8
    }
}

fn check_prob(name: &str, p: f64) -> Result<(), String> {
    if (0.0..=1.0).contains(&p) {
        Ok(())
    } else {
        Err(format!("{name} must be in [0, 1], got {p}"))
    }
}

impl Workload {
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let w: Workload = toml::from_str(s).map_err(|e| format!("invalid workload TOML: {e}"))?;
        w.validate()?;
        Ok(w)
    }

    pub fn builtin_default() -> Self {
        Self::from_toml(DEFAULT_WORKLOAD_TOML).expect("built-in default workload is valid")
    }

    pub fn total_pages(&self) -> u64 {
        self.num_tables as u64 * self.pages_per_table as u64
    }

    /// The phase active at `tick` (1-based).
    pub fn phase_at(&self, tick: u64) -> &Phase {
        self.phases
            .iter()
            .rev()
            .find(|p| p.start_tick <= tick)
            .unwrap_or(&self.phases[0])
    }

    /// Number of hot pages at the start of each table for `phase`.
    pub fn hot_pages(&self, phase: &Phase) -> u32 {
        ((self.pages_per_table as f64 * phase.hot_fraction) as u32).max(1)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "workload name must match [A-Za-z0-9_-]+, got {:?}",
                self.name
            ));
        }
        if self.num_tables == 0 {
            return Err("num_tables must be at least 1".into());
        }
        if self.pages_per_table == 0 {
            return Err("pages_per_table must be at least 1".into());
        }
        check_prob("write_prob", self.write_prob)?;
        let c = &self.cost;
        for (name, v) in [
            ("hit", c.hit),
            ("miss", c.miss),
            ("seek_max", c.seek_max),
            ("dirty_writeback", c.dirty_writeback),
        ] {
            if !(v.is_finite() && v >= 0.0) {
                return Err(format!("cost.{name} must be finite and >= 0, got {v}"));
            }
        }

        match (&self.explicit_pages, self.phases.is_empty()) {
            (Some(_), false) => {
                return Err("use either [[phase]] blocks or explicit_pages, not both".into());
            }
            (None, true) => {
                return Err("workload needs at least one [[phase]] or explicit_pages".into());
            }
            (Some(pages), true) => {
                if pages.is_empty() {
                    return Err("explicit_pages must not be empty".into());
                }
                if let Some(&p) = pages.iter().find(|&&p| p as u64 >= self.total_pages()) {
                    return Err(format!(
                        "explicit page {p} out of range (total pages {})",
                        self.total_pages()
                    ));
                }
            }
            (None, false) => {}
        }

        let mut prev_start = 0;
        for (i, ph) in self.phases.iter().enumerate() {
            let at = format!("phase {}", i + 1);
            if i == 0 && ph.start_tick > 1 {
                return Err(format!("{at}: first phase must start at tick 1"));
            }
            if i > 0 && ph.start_tick <= prev_start {
                return Err(format!(
                    "{at}: start_tick must be greater than the previous phase's"
                ));
            }
            prev_start = ph.start_tick;
            check_prob(&format!("{at}: hot_prob"), ph.hot_prob)?;
            check_prob(&format!("{at}: scan_prob"), ph.scan_prob)?;
            if !(ph.hot_fraction > 0.0 && ph.hot_fraction <= 1.0) {
                return Err(format!(
                    "{at}: hot_fraction must be in (0, 1], got {}",
                    ph.hot_fraction
                ));
            }
            let [lo, hi] = ph.pages_per_query;
            if lo == 0 || lo > hi {
                return Err(format!(
                    "{at}: pages_per_query must be [min, max] with 1 <= min <= max"
                ));
            }
            if ph.scan_prob > 0.0 && (ph.scan_len == 0 || ph.scan_len > self.pages_per_table) {
                return Err(format!("{at}: scan_len must be in 1..=pages_per_table"));
            }
            if ph.queries.is_empty() {
                return Err(format!("{at}: needs at least one [[phase.query]]"));
            }
            for q in &ph.queries {
                if q.tables.is_empty() {
                    return Err(format!("{at}: a query has no tables"));
                }
                if let Some(&t) = q.tables.iter().find(|&&t| t >= self.num_tables) {
                    return Err(format!(
                        "{at}: table {t} out of range (num_tables {})",
                        self.num_tables
                    ));
                }
                if !(q.weight.is_finite() && q.weight > 0.0) {
                    return Err(format!("{at}: query weights must be > 0"));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_default_parses() {
        let w = Workload::builtin_default();
        assert_eq!(w.num_tables, 6);
        assert_eq!(w.phases.len(), 1);
        assert_eq!(w.phases[0].queries.len(), 4);
    }

    #[test]
    fn rejects_out_of_range_table() {
        let s = r#"
            name = "bad"
            num_tables = 2
            pages_per_table = 10
            [[phase]]
            [[phase.query]]
            tables = [0, 5]
            weight = 1.0
        "#;
        let err = Workload::from_toml(s).unwrap_err();
        assert!(err.contains("table 5 out of range"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let s = r#"
            name = "bad"
            num_tables = 2
            pages_per_table = 10
            typo_field = 3
            explicit_pages = [0]
        "#;
        assert!(Workload::from_toml(s).is_err());
    }

    #[test]
    fn phase_lookup() {
        let s = r#"
            name = "p"
            num_tables = 2
            pages_per_table = 10
            [[phase]]
            [[phase.query]]
            tables = [0]
            weight = 1.0
            [[phase]]
            start_tick = 100
            [[phase.query]]
            tables = [1]
            weight = 1.0
        "#;
        let w = Workload::from_toml(s).unwrap();
        assert_eq!(w.phase_at(1).queries[0].tables, vec![0]);
        assert_eq!(w.phase_at(99).queries[0].tables, vec![0]);
        assert_eq!(w.phase_at(100).queries[0].tables, vec![1]);
    }
}
