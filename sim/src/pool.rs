//! The buffer pool: frames, the page table, and the policy that picks victims.
//! It knows nothing about time or storage; the simulator prices what it does.

use std::collections::HashMap;

use serde::Serialize;

use crate::policy::{Ctx, FrameState, Log, Policy};
use crate::trace::Access;
use crate::{FrameIdx, PageId};

#[derive(Clone, Debug)]
struct Frame {
    page: Option<PageId>,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Hit,
    Miss { evicted: Option<Evicted> },
}

/// The page a miss pushed out of its frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Evicted {
    pub page: PageId,
    pub dirty: bool,
}

/// Fixed-size page cache. Owns frames and the page table; delegates victim
/// choice to a [`Policy`].
pub struct BufferPool {
    frames: Vec<Frame>,
    page_table: HashMap<PageId, FrameIdx>,
    next_free: usize,
    policy: Box<dyn Policy>,
}

impl BufferPool {
    pub fn new(num_frames: usize, policy: Box<dyn Policy>) -> Self {
        Self {
            frames: vec![
                Frame {
                    page: None,
                    dirty: false
                };
                num_frames
            ],
            page_table: HashMap::new(),
            next_free: 0,
            policy,
        }
    }

    /// Serve one access; returns the frame that now holds the page.
    pub fn access(&mut self, a: Access, ctx: Ctx, log: &mut Log) -> (FrameIdx, Outcome) {
        if let Some(&f) = self.page_table.get(&a.page) {
            if a.write {
                self.frames[f].dirty = true;
            }
            self.policy.on_hit(f, ctx, log);
            return (f, Outcome::Hit);
        }

        let (f, evicted) = self.place(a.page, a.write, None, ctx, log);
        self.policy.on_load(f, ctx, log);
        (f, Outcome::Miss { evicted })
    }

    /// Whether `page` is in the pool.
    pub fn contains(&self, page: PageId) -> bool {
        self.page_table.contains_key(&page)
    }

    /// Load `page`, which no query asked for yet, as part of the read for
    /// `requested`. The requested page stays pinned: no victim is its frame.
    pub fn read_ahead(
        &mut self,
        page: PageId,
        requested: PageId,
        ctx: Ctx,
        log: &mut Log,
    ) -> Option<Evicted> {
        debug_assert!(!self.contains(page));
        let pinned = *self
            .page_table
            .get(&requested)
            .expect("the requested page is loaded before its read-ahead");
        let (f, evicted) = self.place(page, false, Some(pinned), ctx, log);
        self.policy.on_read_ahead(f, ctx, log);
        evicted
    }

    /// The read for `requested` is done, read-ahead included.
    pub fn request_done(&mut self, requested: PageId) {
        let f = self.page_table[&requested];
        self.policy.on_request_done(f);
    }

    /// Put `page` in a free frame, or in a victim's.
    fn place(
        &mut self,
        page: PageId,
        dirty: bool,
        pinned: Option<FrameIdx>,
        ctx: Ctx,
        log: &mut Log,
    ) -> (FrameIdx, Option<Evicted>) {
        let f = if self.next_free < self.frames.len() {
            self.next_free += 1;
            self.next_free - 1
        } else {
            self.policy.victim(ctx, pinned, log)
        };
        let old = std::mem::replace(
            &mut self.frames[f],
            Frame {
                page: Some(page),
                dirty,
            },
        );
        let evicted = old.page.map(|page| {
            self.page_table.remove(&page);
            Evicted {
                page,
                dirty: old.dirty,
            }
        });
        self.page_table.insert(page, f);
        (f, evicted)
    }

    pub fn policy(&self) -> &dyn Policy {
        self.policy.as_ref()
    }

    /// Every frame: its page, and the policy's view of it.
    pub fn frame_views(&self) -> Vec<FrameView> {
        self.frames
            .iter()
            .enumerate()
            .map(|(i, f)| FrameView {
                page: f.page,
                dirty: f.dirty,
                state: match f.page {
                    Some(_) => self.policy.state(i),
                    None => FrameState::Empty,
                },
            })
            .collect()
    }

    /// Check that frames and the page table agree. For tests.
    pub fn check_consistency(&self) -> Result<(), String> {
        let mut seen = HashMap::new();
        for (i, f) in self.frames.iter().enumerate() {
            if let Some(p) = f.page {
                if let Some(j) = seen.insert(p, i) {
                    return Err(format!("page {p} in frames {j} and {i}"));
                }
                if self.page_table.get(&p) != Some(&i) {
                    return Err(format!("page table disagrees for page {p}"));
                }
            }
        }
        if seen.len() != self.page_table.len() {
            return Err("page table has entries for non-resident pages".into());
        }
        Ok(())
    }
}

/// One frame as an event sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FrameView {
    pub page: Option<PageId>,
    pub dirty: bool,
    pub state: FrameState,
}
