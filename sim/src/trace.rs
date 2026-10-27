//! Access traces: the full, pre-generated stream of page accesses a run
//! replays. Generating the trace up front (rather than sampling during the
//! run) is what lets the optimal policy look into the future and guarantees
//! every policy/layout variant sees the identical stream.

use std::collections::HashMap;
use std::ops::Range;

use rand::Rng;

use crate::workload::{Phase, Workload};
use crate::{PageId, TableId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub page: PageId,
    pub write: bool,
}

/// A sequence of queries, each a run of page accesses. One query = one tick.
#[derive(Clone, Debug)]
pub struct Trace {
    pub num_tables: u16,
    pub pages_per_table: u32,
    accesses: Vec<Access>,
    /// Start offset of each query in `accesses`, plus a trailing sentinel.
    query_starts: Vec<usize>,
}

impl Trace {
    pub fn from_queries(num_tables: u16, pages_per_table: u32, queries: Vec<Vec<Access>>) -> Self {
        let mut accesses = Vec::new();
        let mut query_starts = Vec::with_capacity(queries.len() + 1);
        for q in queries {
            query_starts.push(accesses.len());
            accesses.extend(q);
        }
        query_starts.push(accesses.len());
        Self {
            num_tables,
            pages_per_table,
            accesses,
            query_starts,
        }
    }

    /// Build the trace for `w`. Synthetic workloads produce `ticks` queries;
    /// explicit workloads produce one query per listed page and ignore `ticks`.
    pub fn generate<R: Rng>(w: &Workload, rng: &mut R, ticks: u64) -> Self {
        let queries = match &w.explicit_pages {
            Some(pages) => pages
                .iter()
                .map(|&page| vec![Access { page, write: false }])
                .collect(),
            None => (1..=ticks)
                .map(|t| sample_query(w, w.phase_at(t), rng))
                .collect(),
        };
        Self::from_queries(w.num_tables, w.pages_per_table, queries)
    }

    pub fn num_queries(&self) -> usize {
        self.query_starts.len() - 1
    }

    pub fn accesses(&self) -> &[Access] {
        &self.accesses
    }

    /// Index range of query `q` (0-based) within [`Trace::accesses`].
    pub fn query_bounds(&self, q: usize) -> Range<usize> {
        self.query_starts[q]..self.query_starts[q + 1]
    }

    pub fn table_of(&self, page: PageId) -> TableId {
        (page / self.pages_per_table) as TableId
    }

    /// `next_use[i]` is the index of the next access to the same page after
    /// access `i`, or `usize::MAX` if it is never accessed again.
    pub fn next_use(&self) -> Vec<usize> {
        let mut next = vec![usize::MAX; self.accesses.len()];
        let mut seen: HashMap<PageId, usize> = HashMap::new();
        for (i, a) in self.accesses.iter().enumerate().rev() {
            if let Some(&j) = seen.get(&a.page) {
                next[i] = j;
            }
            seen.insert(a.page, i);
        }
        next
    }
}

fn page_id(w: &Workload, table: TableId, idx: u32) -> PageId {
    table as u32 * w.pages_per_table + idx
}

/// Sample one query. The order of RNG calls here matches the original v0.1
/// simulator exactly (see the seed-42 regression test); change it only on
/// purpose.
fn sample_query<R: Rng>(w: &Workload, phase: &Phase, rng: &mut R) -> Vec<Access> {
    let total: f64 = phase.queries.iter().map(|q| q.weight).sum();
    let roll: f64 = rng.gen_range(0.0..1.0) * total;
    let mut chosen = phase.queries.last().expect("validated: phase has queries");
    let mut cum = 0.0;
    for q in &phase.queries {
        cum += q.weight;
        if roll < cum {
            chosen = q;
            break;
        }
    }

    let hot = rng.gen_bool(phase.hot_prob);
    let max_idx = if hot {
        w.hot_pages(phase)
    } else {
        w.pages_per_table
    };

    let [lo, hi] = phase.pages_per_query;
    let counts: Vec<u32> = chosen
        .tables
        .iter()
        .map(|_| rng.gen_range(lo..hi + 1))
        .collect();

    let mut pages = Vec::new();
    for (&table, &n) in chosen.tables.iter().zip(&counts) {
        for _ in 0..n {
            pages.push(page_id(w, table, rng.gen_range(0..max_idx)));
        }
    }

    if rng.gen_bool(phase.scan_prob) {
        let table = rng.gen_range(0..w.num_tables);
        // Exclusive upper bound kept from v0.1 so seeded runs reproduce.
        let start = rng.gen_range(0..w.pages_per_table.saturating_sub(phase.scan_len).max(1));
        for i in 0..phase.scan_len {
            pages.push(page_id(w, table, start + i));
        }
    }

    pages
        .into_iter()
        .map(|page| Access {
            page,
            write: rng.gen_bool(w.write_prob),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_use_links_repeated_pages() {
        let a = |page| Access { page, write: false };
        let t = Trace::from_queries(1, 10, vec![vec![a(1), a(2)], vec![a(1)], vec![a(3), a(2)]]);
        assert_eq!(t.next_use(), vec![2, 4, usize::MAX, usize::MAX, usize::MAX]);
        assert_eq!(t.num_queries(), 3);
        assert_eq!(t.query_bounds(2), 3..5);
    }

    #[test]
    fn explicit_workload_one_query_per_page() {
        let w = Workload::from_toml(
            "name = \"e\"\nnum_tables = 1\npages_per_table = 5\nexplicit_pages = [0, 1, 0]\n",
        )
        .unwrap();
        let mut rng = rand::rngs::mock::StepRng::new(0, 1);
        let t = Trace::generate(&w, &mut rng, 999);
        assert_eq!(t.num_queries(), 3);
        assert_eq!(
            t.accesses().iter().map(|a| a.page).collect::<Vec<_>>(),
            vec![0, 1, 0]
        );
    }
}
