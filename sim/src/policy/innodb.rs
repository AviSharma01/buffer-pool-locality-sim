//! InnoDB-style: one LRU list split into a young head and an old tail.
//!
//! A miniature of MySQL InnoDB's midpoint insertion (storage/innobase/buf/buf0lru.cc):
//! - The old sublist is the tail 3/8 of the list (innodb_old_blocks_pct = 37),
//!   so the boundary moves as the list grows while the pool fills.
//! - A new page enters at the head of the old sublist, not the head of the list,
//!   so a page read once ages out from the tail without pushing young pages out.
//! - An old page moves to the head of young only when touched again in a later
//!   query. This stands in for innodb_old_blocks_time (1 s): a scan touches a
//!   page several times in quick succession, and those touches must not count.
//! - A young page that is hit moves to the head of the list.
//! - The victim is the tail of the list.
//!
//! Miniature: InnoDB has no old sublist until the list holds 512 pages and
//! skips moving pages already in the youngest quarter; here the split applies
//! from the first page and every young hit moves to the head.

use super::{Ctx, FrameState, Log, Note, Policy, Region, Why};
use crate::FrameIdx;

pub struct InnoDb {
    /// Frames from head (most protected) to tail (next victim).
    list: Vec<FrameIdx>,
    loaded_in: Vec<usize>,
    /// Region of each listed frame as of the last change.
    region: Vec<Region>,
}

/// Old sublist length for a list of `len` pages: 3/8 rounded, and at least
/// one page (when there is one) so a new page always has an old head to enter at.
fn old_len(len: usize) -> usize {
    ((len * 3 + 4) / 8).clamp(len.min(1), len)
}

impl InnoDb {
    pub fn new(frames: usize) -> Self {
        Self {
            list: Vec::with_capacity(frames),
            loaded_in: vec![0; frames],
            region: vec![Region::Young; frames],
        }
    }

    fn position(&self, frame: FrameIdx) -> usize {
        self.list
            .iter()
            .position(|&f| f == frame)
            .expect("frame is in the list")
    }

    fn move_to_head(&mut self, frame: FrameIdx) {
        let i = self.position(frame);
        self.list.remove(i);
        self.list.insert(0, frame);
    }

    /// Recompute regions after a move and note frames the boundary passed
    /// (other than `moved`, whose own note already says where it went).
    fn relabel(&mut self, moved: FrameIdx, log: &mut Log) {
        let boundary = self.list.len() - old_len(self.list.len());
        for (i, &frame) in self.list.iter().enumerate() {
            let region = if i < boundary {
                Region::Young
            } else {
                Region::Old
            };
            if self.region[frame] != region && frame != moved {
                log.push(Note::Relabeled { frame, region });
            }
            self.region[frame] = region;
        }
    }
}

impl Policy for InnoDb {
    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        match self.region[frame] {
            Region::Young => self.move_to_head(frame),
            Region::Old if ctx.query != self.loaded_in[frame] => {
                self.move_to_head(frame);
                log.push(Note::Promoted { frame });
            }
            Region::Old => log.push(Note::StaysOld { frame }),
        }
        self.relabel(frame, log);
    }

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        let len = self.list.len() + 1;
        let midpoint = len - old_len(len);
        self.list.insert(midpoint, frame);
        self.loaded_in[frame] = ctx.query;
        log.push(Note::Inserted { frame });
        self.relabel(frame, log);
    }

    fn victim(&mut self, _ctx: Ctx, log: &mut Log) -> FrameIdx {
        let frame = self.list.pop().expect("pool has frames");
        log.push(Note::Evict {
            frame,
            why: Why::OldTail,
        });
        frame
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        FrameState::InnoDb {
            region: self.region[frame],
            rank: self.list.len() - 1 - self.position(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::test_util::{at, log};

    fn filled(frames: usize) -> InnoDb {
        let mut p = InnoDb::new(frames);
        for f in 0..frames {
            p.on_load(f, at(f, 0), &mut log());
        }
        p
    }

    #[test]
    fn new_pages_enter_at_the_head_of_old() {
        let mut p = filled(8);
        // 3 of 8 frames are old; frame 7 was loaded last, so it heads old.
        assert_eq!(old_len(8), 3);
        assert_eq!(
            p.state(7),
            FrameState::InnoDb {
                region: Region::Old,
                rank: 2
            }
        );
        let mut l = log();
        let v = p.victim(at(8, 1), &mut l);
        p.on_load(v, at(8, 1), &mut l);
        let notes = l.take();
        assert_eq!(notes[1], Note::Inserted { frame: v });
        assert_eq!(notes.len(), 2, "a full pool keeps the boundary: {notes:?}");
        assert_eq!(
            p.state(v),
            FrameState::InnoDb {
                region: Region::Old,
                rank: 2
            }
        );
    }

    #[test]
    fn old_pages_are_promoted_only_by_a_later_query() {
        let mut p = filled(8);
        let old = p.list[7];
        let mut l = log();
        p.on_hit(old, at(8, 0), &mut l);
        assert_eq!(l.take(), [Note::StaysOld { frame: old }]);

        p.on_hit(old, at(9, 1), &mut l);
        let notes = l.take();
        assert_eq!(notes[0], Note::Promoted { frame: old });
        // The young tail is pushed across the boundary into old.
        assert!(matches!(
            notes[1],
            Note::Relabeled {
                region: Region::Old,
                ..
            }
        ));
        assert_eq!(
            p.state(old),
            FrameState::InnoDb {
                region: Region::Young,
                rank: 7
            }
        );
    }

    #[test]
    fn a_one_query_scan_only_churns_the_old_sublist() {
        let mut p = filled(8);
        // Touch every young page again so they look hot.
        let young: Vec<FrameIdx> = p.list[..5].to_vec();
        for (pos, &f) in young.iter().enumerate() {
            p.on_hit(f, at(10 + pos, 1), &mut log());
        }
        let young_before: Vec<FrameIdx> = p.list[..5].to_vec();
        for pos in 20..40 {
            let ctx = at(pos, 2);
            let f = p.victim(ctx, &mut log());
            p.on_load(f, ctx, &mut log());
            p.on_hit(f, ctx, &mut log());
        }
        assert_eq!(p.list[..5], young_before);
    }
}
