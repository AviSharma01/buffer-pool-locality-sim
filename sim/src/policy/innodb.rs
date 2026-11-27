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
//!
//! Settings: `old=N` makes the old sublist N% of the list (5 to 95, like
//! innodb_old_blocks_pct); `promote=any` moves an old page up on any use
//! after its first, in the same query or not (like innodb_old_blocks_time = 0).

use super::list::FrameList;
use super::{Ctx, FrameState, InnoDbSettings, Log, Note, Policy, Promote, Region, Why};
use crate::FrameIdx;

pub struct InnoDb {
    /// Frames from head (most protected) to tail (next victim).
    list: FrameList,
    /// Head of the old sublist (None while it is empty), how many frames run
    /// from it to the tail, and which listed frames are in it. Kept exact
    /// through every change; `relabel` then moves the head to the 3/8 mark.
    old_head: Option<FrameIdx>,
    old_count: usize,
    in_old: Vec<bool>,
    /// Query of each frame's first use; None for a page read ahead and not
    /// used yet (innodb_old_blocks_time counts from the first access).
    loaded_in: Vec<Option<usize>>,
    /// Region of each listed frame as of the last relabel.
    region: Vec<Region>,
    /// Frames that crossed the boundary since the last relabel.
    crossed: Vec<FrameIdx>,
    settings: InnoDbSettings,
}

/// Old sublist length for a list of `len` pages: 3/8 (or `pct`%) rounded, and
/// at least one page (when there is one) so a new page always has an old head
/// to enter at.
fn old_len(len: usize, pct: Option<u8>) -> usize {
    let old = match pct {
        None => (len * 3 + 4) / 8,
        Some(pct) => (len * pct as usize + 50) / 100,
    };
    old.clamp(len.min(1), len)
}

impl InnoDb {
    pub fn new(frames: usize) -> Self {
        Self::with(frames, InnoDbSettings::DEFAULT)
    }

    pub fn with(frames: usize, settings: InnoDbSettings) -> Self {
        assert!(
            settings
                .old_pct
                .is_none_or(|p| InnoDbSettings::OLD_PCT.contains(&p)),
            "old sublist share out of range"
        );
        Self {
            settings,
            list: FrameList::new(frames),
            old_head: None,
            old_count: 0,
            in_old: vec![false; frames],
            loaded_in: vec![None; frames],
            region: vec![Region::Young; frames],
            crossed: Vec::new(),
        }
    }

    fn remove(&mut self, frame: FrameIdx) {
        if self.in_old[frame] {
            self.in_old[frame] = false;
            self.old_count -= 1;
            if self.old_head == Some(frame) {
                self.old_head = self.list.next(frame);
            }
        }
        self.list.remove(frame);
    }

    fn move_to_head(&mut self, frame: FrameIdx) {
        self.remove(frame);
        self.list.push_front(frame);
    }

    /// Move the old head until `count` frames run from it to the tail.
    fn set_old_count(&mut self, count: usize) {
        while self.old_count > count {
            let frame = self.old_head.expect("old sublist is not empty");
            self.cross(frame, false);
            self.old_head = self.list.next(frame);
            self.old_count -= 1;
        }
        while self.old_count < count {
            let frame = match self.old_head {
                Some(head) => self.list.prev(head),
                None => self.list.back(),
            }
            .expect("list is long enough");
            self.cross(frame, true);
            self.old_head = Some(frame);
            self.old_count += 1;
        }
    }

    fn cross(&mut self, frame: FrameIdx, old: bool) {
        self.in_old[frame] = old;
        self.crossed.push(frame);
    }

    fn side(&self, frame: FrameIdx) -> Region {
        if self.in_old[frame] {
            Region::Old
        } else {
            Region::Young
        }
    }

    /// Put the boundary at the 3/8 mark and note frames it passed (other
    /// than `moved`, whose own note already says where it went).
    fn relabel(&mut self, moved: FrameIdx, log: &mut Log) {
        self.set_old_count(old_len(self.list.len(), self.settings.old_pct));
        self.region[moved] = self.side(moved);
        let mut crossed = std::mem::take(&mut self.crossed);
        for &frame in &crossed {
            let region = self.side(frame);
            if frame != moved && self.list.contains(frame) && self.region[frame] != region {
                log.push(Note::Relabeled { frame, region });
                self.region[frame] = region;
            }
        }
        crossed.clear();
        self.crossed = crossed;
        debug_assert!(
            self.list.iter().all(|f| self.region[f] == self.side(f)),
            "a frame's region went stale"
        );
    }
}

impl Policy for InnoDb {
    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        match (self.region[frame], self.loaded_in[frame]) {
            (Region::Young, _) => self.move_to_head(frame),
            (Region::Old, None) => {
                self.loaded_in[frame] = Some(ctx.query);
                log.push(Note::FirstUse { frame });
            }
            (Region::Old, Some(q))
                if q != ctx.query || self.settings.promote == Promote::AnyReuse =>
            {
                self.move_to_head(frame);
                log.push(Note::Promoted { frame });
            }
            (Region::Old, Some(_)) => log.push(Note::StaysOld { frame }),
        }
        self.relabel(frame, log);
    }

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        // Enter as the head of old: old_len of the longer list, counting itself.
        self.set_old_count(old_len(self.list.len() + 1, self.settings.old_pct) - 1);
        match self.old_head {
            Some(head) => self.list.insert_before(frame, head),
            None => self.list.push_back(frame),
        }
        self.in_old[frame] = true;
        self.old_head = Some(frame);
        self.old_count += 1;
        self.loaded_in[frame] = Some(ctx.query);
        log.push(Note::Inserted { frame });
        self.relabel(frame, log);
    }

    /// Read ahead: also enters at the head of old, but counts as not used
    /// yet, so its first real use does not promote it.
    fn on_read_ahead(&mut self, frame: FrameIdx, ctx: Ctx, log: &mut Log) {
        self.on_load(frame, ctx, log);
        self.loaded_in[frame] = None;
    }

    fn victim(&mut self, _ctx: Ctx, pinned: Option<FrameIdx>, log: &mut Log) -> FrameIdx {
        // The tail, or the frame before it if the tail is pinned.
        let frame = self
            .list
            .back_except(pinned)
            .expect("pool has an unpinned frame");
        self.remove(frame);
        log.push(Note::Evict {
            frame,
            why: Why::OldTail,
        });
        frame
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        FrameState::InnoDb {
            region: self.region[frame],
            rank: self.list.behind(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::test_util::{at, log};

    fn order(p: &InnoDb) -> Vec<FrameIdx> {
        p.list.iter().collect()
    }

    fn filled(frames: usize) -> InnoDb {
        filled_with(frames, InnoDbSettings::DEFAULT)
    }

    fn filled_with(frames: usize, settings: InnoDbSettings) -> InnoDb {
        let mut p = InnoDb::with(frames, settings);
        for f in 0..frames {
            p.on_load(f, at(f, 0), &mut log());
        }
        p
    }

    #[test]
    fn new_pages_enter_at_the_head_of_old() {
        let mut p = filled(8);
        // 3 of 8 frames are old; frame 7 was loaded last, so it heads old.
        assert_eq!(old_len(8, None), 3);
        assert_eq!(
            p.state(7),
            FrameState::InnoDb {
                region: Region::Old,
                rank: 2
            }
        );
        let mut l = log();
        let v = p.victim(at(8, 1), None, &mut l);
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
        let old = order(&p)[7];
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
    fn with_promote_any_a_re_use_in_the_same_query_promotes() {
        let settings = InnoDbSettings {
            promote: Promote::AnyReuse,
            ..InnoDbSettings::DEFAULT
        };
        let mut p = filled_with(8, settings);
        let old = order(&p)[7];
        let mut l = log();
        p.on_hit(old, at(8, 0), &mut l);
        assert_eq!(l.take()[0], Note::Promoted { frame: old });
        assert_eq!(
            p.state(old),
            FrameState::InnoDb {
                region: Region::Young,
                rank: 7
            }
        );
    }

    #[test]
    fn with_promote_any_a_page_read_ahead_still_needs_a_second_use() {
        let settings = InnoDbSettings {
            promote: Promote::AnyReuse,
            ..InnoDbSettings::DEFAULT
        };
        let mut p = InnoDb::with(8, settings);
        let mut l = log();
        for f in 0..7 {
            p.on_load(f, at(f, 0), &mut l);
        }
        p.on_read_ahead(7, at(7, 0), &mut l);
        l.take();
        p.on_hit(7, at(8, 0), &mut l);
        assert_eq!(l.take(), [Note::FirstUse { frame: 7 }]);
        p.on_hit(7, at(9, 0), &mut l);
        assert_eq!(l.take()[0], Note::Promoted { frame: 7 });
    }

    #[test]
    fn the_old_share_follows_its_setting() {
        assert_eq!(old_len(100, None), 38, "3/8 rounded");
        assert_eq!(old_len(100, Some(37)), 37);
        assert_eq!(old_len(100, Some(5)), 5);
        assert_eq!(old_len(100, Some(95)), 95);
        assert_eq!(old_len(10, Some(5)), 1, "at least one old page");
        assert_eq!(old_len(10, Some(95)), 10, "rounded, never past the list");
        assert_eq!(old_len(0, Some(50)), 0);
        for pct in [5, 20, 50, 95] {
            let settings = InnoDbSettings {
                old_pct: Some(pct),
                ..InnoDbSettings::DEFAULT
            };
            let p = filled_with(40, settings);
            let old = (0..40).filter(|&f| p.region[f] == Region::Old).count();
            assert_eq!(old, old_len(40, Some(pct)), "old {pct}%");
        }
    }

    #[test]
    fn a_one_query_scan_only_churns_the_old_sublist() {
        let mut p = filled(8);
        // Touch every young page again so they look hot.
        let young = order(&p)[..5].to_vec();
        for (pos, &f) in young.iter().enumerate() {
            p.on_hit(f, at(10 + pos, 1), &mut log());
        }
        let young_before = order(&p)[..5].to_vec();
        for pos in 20..40 {
            let ctx = at(pos, 2);
            let f = p.victim(ctx, None, &mut log());
            p.on_load(f, ctx, &mut log());
            p.on_hit(f, ctx, &mut log());
        }
        assert_eq!(order(&p)[..5], young_before);
    }

    /// A page read ahead also enters at the head of old, but its first use
    /// only starts its clock: a first use in a later query does not promote
    /// it; a second use in a still later query does.
    #[test]
    fn a_page_read_ahead_is_not_promoted_by_its_first_use() {
        let mut p = filled(8);
        let v = p.victim(at(8, 1), None, &mut log());
        p.on_read_ahead(v, at(8, 1), &mut log());
        let mut l = log();
        p.on_hit(v, at(9, 2), &mut l);
        assert_eq!(l.take(), [Note::FirstUse { frame: v }]);
        assert!(matches!(
            p.state(v),
            FrameState::InnoDb {
                region: Region::Old,
                ..
            }
        ));
        p.on_hit(v, at(10, 3), &mut l);
        assert_eq!(l.take()[0], Note::Promoted { frame: v });
    }
}
