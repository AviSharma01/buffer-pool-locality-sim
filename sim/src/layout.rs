//! Physical placement of tables on disk, co-access tracking, and
//! reclustering (Minimum Linear Arrangement by brute force).

use std::collections::BTreeMap;

use rand::Rng;

use crate::{PageId, TableId};

/// Above this many tables, brute-force reclustering (n!) is too slow.
pub const MAX_RECLUSTER_TABLES: u16 = 10;

/// Random initial table order (Fisher-Yates). RNG call order matches v0.1.
pub fn shuffled<R: Rng>(num_tables: u16, rng: &mut R) -> Vec<TableId> {
    let mut order: Vec<TableId> = (0..num_tables).collect();
    for i in (1..num_tables as usize).rev() {
        let j = rng.gen_range(0..=i);
        order.swap(i, j);
    }
    order
}

/// Tables laid out contiguously on disk in `order` (slot index -> table).
#[derive(Clone, Debug)]
pub struct Layout {
    order: Vec<TableId>,
    slot_of: Vec<usize>,
    pages_per_table: u32,
}

impl Layout {
    pub fn new(order: Vec<TableId>, pages_per_table: u32) -> Self {
        let mut slot_of = vec![0; order.len()];
        for (slot, &t) in order.iter().enumerate() {
            slot_of[t as usize] = slot;
        }
        Self {
            order,
            slot_of,
            pages_per_table,
        }
    }

    pub fn order(&self) -> &[TableId] {
        &self.order
    }

    pub fn slot(&self, table: TableId) -> usize {
        self.slot_of[table as usize]
    }

    /// Physical disk address of `page` under this layout.
    pub fn disk_pos(&self, page: PageId) -> u64 {
        let ppt = self.pages_per_table;
        let table = (page / ppt) as TableId;
        self.slot(table) as u64 * ppt as u64 + (page % ppt) as u64
    }
}

/// How often each pair of tables is touched by the same query.
#[derive(Clone, Debug, Default)]
pub struct CoAccess {
    weights: BTreeMap<(TableId, TableId), f64>,
}

impl CoAccess {
    /// Record one query that touched `tables` (sorted, deduplicated).
    pub fn record(&mut self, tables: &[TableId]) {
        for i in 0..tables.len() {
            for j in (i + 1)..tables.len() {
                *self.weights.entry((tables[i], tables[j])).or_insert(0.0) += 1.0;
            }
        }
    }

    /// Multiply all weights by `factor` so older history counts for less.
    pub fn decay(&mut self, factor: f64) {
        if factor < 1.0 {
            self.weights.values_mut().for_each(|w| *w *= factor);
        }
    }

    pub fn pairs(&self) -> impl Iterator<Item = (TableId, TableId, f64)> + '_ {
        self.weights.iter().map(|(&(a, b), &w)| (a, b, w))
    }
}

/// MLA objective: sum over pairs of weight(a, b) * |slot(a) - slot(b)|.
pub fn arrangement_cost(order: &[TableId], co: &CoAccess) -> f64 {
    let mut pos = vec![0usize; order.len()];
    for (slot, &t) in order.iter().enumerate() {
        pos[t as usize] = slot;
    }
    co.pairs()
        .map(|(a, b, w)| w * pos[a as usize].abs_diff(pos[b as usize]) as f64)
        .sum()
}

/// Exhaustively find the table order minimising [`arrangement_cost`].
/// Ties go to the lexicographically first order.
pub fn optimal_order(num_tables: u16, co: &CoAccess) -> Vec<TableId> {
    let mut perm: Vec<TableId> = (0..num_tables).collect();
    let mut best = perm.clone();
    let mut best_cost = arrangement_cost(&perm, co);
    while next_permutation(&mut perm) {
        let c = arrangement_cost(&perm, co);
        if c < best_cost {
            best_cost = c;
            best.copy_from_slice(&perm);
        }
    }
    best
}

/// Advance `arr` to the next lexicographic permutation in place. Returns
/// false (leaving `arr` unchanged) once it is the last permutation.
pub fn next_permutation(arr: &mut [TableId]) -> bool {
    let n = arr.len();
    if n < 2 {
        return false;
    }
    let mut i = n - 1;
    while i > 0 && arr[i - 1] >= arr[i] {
        i -= 1;
    }
    if i == 0 {
        return false;
    }
    let mut j = n - 1;
    while arr[j] <= arr[i - 1] {
        j -= 1;
    }
    arr.swap(i - 1, j);
    arr[i..].reverse();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permutations_count() {
        let mut p: Vec<TableId> = (0..6).collect();
        let mut n = 1;
        while next_permutation(&mut p) {
            n += 1;
        }
        assert_eq!(n, 720);
    }

    #[test]
    fn chain_is_laid_out_in_order() {
        // 2-0, 0-3, 3-1 form a chain; the optimal order puts it in a line.
        let mut co = CoAccess::default();
        for _ in 0..10 {
            co.record(&[0, 2]);
            co.record(&[0, 3]);
            co.record(&[1, 3]);
        }
        let order = optimal_order(4, &co);
        assert_eq!(arrangement_cost(&order, &co), 30.0);
        assert!(
            order == vec![1, 3, 0, 2] || order == vec![2, 0, 3, 1],
            "{order:?}"
        );
    }

    #[test]
    fn disk_positions_follow_layout() {
        let l = Layout::new(vec![2, 0, 1], 10);
        assert_eq!(l.disk_pos(25), 5); // table 2 in slot 0
        assert_eq!(l.disk_pos(3), 13); // table 0 in slot 1
        assert_eq!(l.disk_pos(10), 20); // table 1 in slot 2
    }

    #[test]
    fn decay_scales_weights() {
        let mut co = CoAccess::default();
        co.record(&[0, 1]);
        co.record(&[0, 1]);
        co.decay(0.5);
        assert_eq!(co.pairs().collect::<Vec<_>>(), vec![(0, 1, 1.0)]);
    }
}
