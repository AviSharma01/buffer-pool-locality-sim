//! PostgreSQL-style: clock sweep over usage counts, plus a ring for big scans.
//!
//! A miniature of PostgreSQL's buffer replacement (storage/buffer/freelist.c):
//! - A newly loaded page starts with usage count 1; each hit adds 1, up to 5.
//! - To find a victim the hand sweeps the pool, lowering each count it passes,
//!   and takes the first frame already at 0. A page hit often survives several
//!   sweeps, so popular pages stay.
//! - A sequential scan of a relation bigger than a quarter of the pool gets a
//!   small ring of frames (PostgreSQL's bulk-read strategy) and recycles those
//!   instead of sweeping the rest of the pool. A ring frame is recycled only if
//!   its usage count is at most 1; if another query made it hot, the scan
//!   sweeps for a frame and that frame takes the ring slot. The scan's own hits
//!   only raise a count from 0 to 1, so a scan cannot make pages look hot.
//!
//! Miniature: the ring is a quarter of the pool (PostgreSQL: 32 frames, at most
//! an eighth of shared_buffers) and lasts for one query.

use super::{Ctx, FrameState, Log, Note, Policy, Why};
use crate::FrameIdx;

const MAX_USAGE: u8 = 5;

pub struct Pg {
    usage: Vec<u8>,
    hand: FrameIdx,
    /// Ring slots of the current query's scan; None until the scan fills it.
    ring: Vec<Option<FrameIdx>>,
    /// Slot the scan used last.
    cursor: usize,
    ring_query: Option<usize>,
}

impl Pg {
    pub fn new(frames: usize) -> Self {
        let ring_len = (frames / 4).max(1);
        Self {
            usage: vec![0; frames],
            hand: 0,
            ring: vec![None; ring_len],
            cursor: ring_len - 1,
            ring_query: None,
        }
    }

    /// Whether this access goes through the scan ring. Drops the previous
    /// query's ring first.
    fn ring_applies(&mut self, ctx: Ctx) -> bool {
        if self.ring_query.is_some_and(|q| q != ctx.query) {
            self.ring.fill(None);
            self.cursor = self.ring.len() - 1;
            self.ring_query = None;
        }
        let big_scan = matches!(ctx.scan, Some(pages) if pages as usize > self.usage.len() / 4);
        if big_scan {
            self.ring_query = Some(ctx.query);
        }
        big_scan
    }

    fn next_slot(&self) -> usize {
        (self.cursor + 1) % self.ring.len()
    }

    fn set_usage(&mut self, frame: FrameIdx, to: u8, log: &mut Log) {
        let from = self.usage[frame];
        if from != to {
            self.usage[frame] = to;
            log.push(Note::Counted { frame, from, to });
        }
    }

    fn sweep(&mut self, log: &mut Log) -> FrameIdx {
        let n = self.usage.len();
        loop {
            let frame = self.hand;
            self.hand = (self.hand + 1) % n;
            let from = self.usage[frame];
            if from == 0 {
                log.push(Note::Evict {
                    frame,
                    why: Why::CountZero,
                });
                return frame;
            }
            self.usage[frame] = from - 1;
            log.push(Note::Swept {
                frame,
                from,
                to: from - 1,
            });
        }
    }
}

impl Policy for Pg {
    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        let to = if self.ring_applies(ctx) {
            self.usage[frame].max(1)
        } else {
            (self.usage[frame] + 1).min(MAX_USAGE)
        };
        self.set_usage(frame, to, log);
    }

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        self.usage[frame] = 0;
        self.set_usage(frame, 1, log);
        if self.ring_applies(ctx) {
            self.cursor = self.next_slot();
            if self.ring[self.cursor] != Some(frame) {
                self.ring[self.cursor] = Some(frame);
                log.push(Note::JoinedRing { frame });
            }
        }
    }

    fn victim(&mut self, ctx: Ctx, log: &mut Log) -> FrameIdx {
        if self.ring_applies(ctx) {
            if let Some(frame) = self.ring[self.next_slot()] {
                let usage = self.usage[frame];
                if usage <= 1 {
                    log.push(Note::Evict {
                        frame,
                        why: Why::RingReuse { usage },
                    });
                    return frame;
                }
                log.push(Note::RingBusy { frame, usage });
            }
        }
        self.sweep(log)
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        FrameState::Pg {
            usage: self.usage[frame],
            in_ring: self.ring.contains(&Some(frame)),
        }
    }

    fn hand(&self) -> Option<FrameIdx> {
        Some(self.hand)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::test_util::{at, log};

    fn scan(pos: usize, query: usize, pages: u32) -> Ctx {
        Ctx {
            pos,
            query,
            scan: Some(pages),
        }
    }

    fn usage(p: &Pg, frame: FrameIdx) -> u8 {
        match p.state(frame) {
            FrameState::Pg { usage, .. } => usage,
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn usage_counts_cap_at_five_and_buy_extra_sweeps() {
        let mut p = Pg::new(2);
        let mut l = log();
        p.on_load(0, at(0, 0), &mut l);
        p.on_load(1, at(1, 0), &mut l);
        for pos in 2..10 {
            p.on_hit(0, at(pos, 0), &mut l);
        }
        assert_eq!(usage(&p, 0), MAX_USAGE);
        l.take();
        // Frame 0 (usage 5) outlives frame 1 (usage 1): 5->4, 1->0, 4->3, evict 1.
        assert_eq!(p.victim(at(10, 0), &mut l), 1);
        let notes = l.take();
        assert_eq!(notes.len(), 4);
        assert_eq!(
            notes[3],
            Note::Evict {
                frame: 1,
                why: Why::CountZero
            }
        );
        assert_eq!(usage(&p, 0), 3);
    }

    #[test]
    fn a_big_scan_recycles_its_ring_and_spares_hot_pages() {
        // 8 frames: ring of 2, used for scans of relations over 2 pages.
        let mut p = Pg::new(8);
        let mut l = log();
        for f in 0..8 {
            p.on_load(f, at(f, 0), &mut l);
            p.on_hit(f, at(f, 0), &mut l);
        }
        let mut victims = Vec::new();
        for pos in 8..14 {
            let f = p.victim(scan(pos, 1, 100), &mut l);
            p.on_load(f, scan(pos, 1, 100), &mut l);
            victims.push(f);
        }
        // Two sweeps fill the ring; after that the scan only recycles those two.
        assert_eq!(victims, [0, 1, 0, 1, 0, 1]);
        assert!(l.take().contains(&Note::Evict {
            frame: 0,
            why: Why::RingReuse { usage: 1 }
        }));
        assert_eq!(
            p.state(1),
            FrameState::Pg {
                usage: 1,
                in_ring: true
            }
        );
        // The next query starts without a ring.
        p.on_hit(5, at(14, 2), &mut l);
        assert_eq!(
            p.state(1),
            FrameState::Pg {
                usage: 1,
                in_ring: false
            }
        );
    }

    #[test]
    fn small_scans_and_hot_ring_frames_use_the_sweep() {
        let mut p = Pg::new(8);
        let mut l = log();
        for f in 0..8 {
            p.on_load(f, at(f, 0), &mut l);
        }
        // A 2-page relation is not bigger than a quarter of 8 frames.
        assert!(!p.ring_applies(scan(8, 1, 2)));

        // The first ring frame comes from a sweep, which leaves every count at 0.
        let f = p.victim(scan(9, 2, 100), &mut l);
        p.on_load(f, scan(9, 2, 100), &mut l);
        assert_eq!(usage(&p, 4), 0);
        // A scan hit lifts a count only to 1; normal hits keep adding.
        p.on_hit(4, scan(10, 2, 100), &mut l);
        p.on_hit(4, scan(11, 2, 100), &mut l);
        assert_eq!(usage(&p, 4), 1);
        // Another access in the same query makes the ring frame hot.
        for pos in 12..15 {
            p.on_hit(f, at(pos, 2), &mut l);
        }
        let g = p.victim(scan(15, 2, 100), &mut l);
        p.on_load(g, scan(15, 2, 100), &mut l);
        l.take();
        // Back at slot 0, frame f has usage 4: too hot to recycle, so sweep.
        let h = p.victim(scan(16, 2, 100), &mut l);
        assert_eq!(l.take()[0], Note::RingBusy { frame: f, usage: 4 });
        assert_ne!(h, f);
    }
}
