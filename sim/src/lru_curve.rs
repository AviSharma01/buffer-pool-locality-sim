//! LRU's misses and disk reads at every pool size, from one pass over a trace.
//!
//! LRU keeps the pages used most recently, so an access hits in a pool of n
//! frames exactly when fewer than n other pages were used since the page's
//! last use (its stack distance; Mattson et al., 1970). Counting the distances
//! once gives every pool size. This is the engine's LRU with read-ahead off:
//! nothing is pinned then, and each miss reads one page, or none for a fresh
//! page. tests/replay.rs checks the two agree at every size.

use crate::trace::Trace;

pub struct LruCurve {
    /// Accesses at stack distance `d` or more, for `d` from 0 up: these miss
    /// in any pool of `d` frames or fewer. First uses are counted apart.
    misses_from: Vec<u64>,
    reads_from: Vec<u64>,
    /// First uses miss in any pool; those of fresh pages read nothing.
    first_misses: u64,
    first_reads: u64,
}

impl LruCurve {
    pub fn new(trace: &Trace) -> Self {
        let accesses = trace.accesses();
        let pages = accesses
            .iter()
            .map(|a| a.page as usize + 1)
            .max()
            .unwrap_or(0);
        let mut last_use = vec![usize::MAX; pages];
        // 1 at each position that is its page's latest use: the pages used
        // between two positions are the 1s between them.
        let mut latest = Fenwick::new(accesses.len());
        let (mut misses, mut reads) = (Vec::<u64>::new(), Vec::<u64>::new());
        let (mut first_misses, mut first_reads) = (0, 0);
        for (i, a) in accesses.iter().enumerate() {
            let page = a.page as usize;
            match std::mem::replace(&mut last_use[page], i) {
                usize::MAX => {
                    first_misses += 1;
                    first_reads += u64::from(!a.fresh);
                }
                j => {
                    let distance = latest.sum_between(j + 1, i);
                    latest.add(j, -1);
                    if misses.len() <= distance {
                        misses.resize(distance + 1, 0);
                        reads.resize(distance + 1, 0);
                    }
                    misses[distance] += 1;
                    reads[distance] += u64::from(!a.fresh);
                }
            }
            latest.add(i, 1);
        }
        Self {
            misses_from: from_each(misses),
            reads_from: from_each(reads),
            first_misses,
            first_reads,
        }
    }

    pub fn misses(&self, frames: usize) -> u64 {
        self.first_misses + self.misses_from.get(frames).copied().unwrap_or(0)
    }

    /// Pages read from disk: misses, less those on fresh pages.
    pub fn disk_reads(&self, frames: usize) -> u64 {
        self.first_reads + self.reads_from.get(frames).copied().unwrap_or(0)
    }

    /// The smallest pool in which LRU reads no more than `reads` pages, or
    /// None when no pool gets that low (every page's first read is a miss).
    pub fn frames_for(&self, reads: u64) -> Option<usize> {
        if reads < self.first_reads {
            return None;
        }
        // disk_reads only falls as the pool grows, and is at its floor once
        // the pool holds every distance seen.
        let (mut lo, mut hi) = (1, self.reads_from.len().max(1));
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.disk_reads(mid) <= reads {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Some(lo)
    }
}

/// `counts[d]` per distance into "this distance or more", with a 0 after the
/// last so any larger pool reads 0 from it.
fn from_each(mut counts: Vec<u64>) -> Vec<u64> {
    let mut total = 0;
    for c in counts.iter_mut().rev() {
        total += *c;
        *c = total;
    }
    counts.push(0);
    counts
}

/// Sums over positions in O(log n).
struct Fenwick(Vec<i64>);

impl Fenwick {
    fn new(n: usize) -> Self {
        Self(vec![0; n + 1])
    }

    fn add(&mut self, at: usize, by: i64) {
        let mut i = at + 1;
        while i < self.0.len() {
            self.0[i] += by;
            i += i & i.wrapping_neg();
        }
    }

    /// Sum of positions before `end`.
    fn before(&self, end: usize) -> i64 {
        let (mut i, mut sum) = (end, 0);
        while i > 0 {
            sum += self.0[i];
            i -= i & i.wrapping_neg();
        }
        sum
    }

    /// Sum of positions `start..end`, as a count.
    fn sum_between(&self, start: usize, end: usize) -> usize {
        usize::try_from(self.before(end) - self.before(start)).expect("a count")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Access, Trace};

    fn trace(pages: &[(u32, bool)]) -> Trace {
        let accesses = pages
            .iter()
            .map(|&(page, fresh)| Access {
                page,
                write: fresh,
                fresh,
                scan: None,
            })
            .collect();
        Trace::from_queries(1, 10, vec![accesses])
    }

    #[test]
    fn counts_misses_by_how_many_pages_came_between() {
        // 1 2 3 1 2 4 1: the second 1 and 2 each have 2 pages between, the
        // last 1 has 2 (2 and 4).
        let t = trace(&[1, 2, 3, 1, 2, 4, 1].map(|p| (p, false)));
        let curve = LruCurve::new(&t);
        let misses: Vec<u64> = (1..=5).map(|f| curve.misses(f)).collect();
        assert_eq!(misses, [7, 7, 4, 4, 4]);
        assert_eq!(curve.frames_for(4), Some(3));
        assert_eq!(curve.frames_for(7), Some(1));
        assert_eq!(curve.frames_for(3), None, "four first uses always miss");
    }

    #[test]
    fn fresh_pages_miss_but_are_not_read() {
        // Page 5 is written new, then used again after 1 other page.
        let t = trace(&[(5, true), (6, false), (5, false), (6, false)]);
        let curve = LruCurve::new(&t);
        assert_eq!((curve.misses(1), curve.disk_reads(1)), (4, 3));
        assert_eq!((curve.misses(2), curve.disk_reads(2)), (2, 1));
        assert_eq!(curve.frames_for(1), Some(2));
    }
}
