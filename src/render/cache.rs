//! GPU texture cache for rendered pages, keyed by (page, scale bucket).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui;

use super::scale_key;

/// Safety net only: the worker answers every request, including the ones it
/// drops, so a pending entry this old means the worker thread is gone.
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

/// Eviction budgets. Counting textures instead of bytes does not work here: a
/// US Letter page at the 8x bucket is ~4900x6300x4 ≈ 124 MB, so a fixed count
/// that is generous at 1x exhausts GPU memory when zoomed in.
pub const CANVAS_BUDGET: usize = 384 << 20;
pub const THUMB_BUDGET: usize = 64 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key {
    page: usize,
    scale: u32,
}

struct Entry {
    tex: egui::TextureHandle,
    bytes: usize,
}

pub struct TextureCache {
    map: HashMap<Key, Entry>,
    /// Most-recently-used at the back.
    lru: Vec<Key>,
    /// Requests sent to the worker but not yet answered.
    pending: HashMap<Key, Instant>,
    /// Keys used while painting this frame. They are never evicted, so the
    /// visible set can't be thrown away by an insert made in the same frame.
    frame_touched: HashSet<Key>,
    budget: usize,
    bytes: usize,
}

impl Default for TextureCache {
    fn default() -> Self {
        Self::with_budget(CANVAS_BUDGET)
    }
}

impl TextureCache {
    pub fn with_budget(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            lru: Vec::new(),
            pending: HashMap::new(),
            frame_touched: HashSet::new(),
            budget,
            bytes: 0,
        }
    }

    /// Called once per frame, before anything paints.
    pub fn begin_frame(&mut self) {
        self.frame_touched.clear();
    }

    fn touch(&mut self, key: Key) {
        self.lru.retain(|k| *k != key);
        self.lru.push(key);
        self.frame_touched.insert(key);
    }

    pub fn get(&mut self, page: usize, scale: f32) -> Option<egui::TextureHandle> {
        let key = Key {
            page,
            scale: scale_key(scale),
        };
        let tex = self.map.get(&key)?.tex.clone();
        self.touch(key);
        Some(tex)
    }

    /// The best available texture for this page at any scale (used while the
    /// exact scale renders), together with its scale. Touches the LRU: a
    /// texture kept only as a fallback is still in use, and must not sink to
    /// the front of the eviction queue while it is the thing on screen.
    pub fn best_effort(&mut self, page: usize) -> Option<(f32, egui::TextureHandle)> {
        let key = *self
            .map
            .keys()
            .filter(|k| k.page == page)
            .max_by_key(|k| k.scale)?;
        let tex = self.map.get(&key)?.tex.clone();
        self.touch(key);
        Some((key.scale as f32 / 100.0, tex))
    }

    pub fn is_pending(&mut self, page: usize, scale: f32) -> bool {
        let key = Key {
            page,
            scale: scale_key(scale),
        };
        match self.pending.get(&key) {
            Some(since) if since.elapsed() < PENDING_TIMEOUT => true,
            Some(_) => {
                self.pending.remove(&key);
                false
            }
            None => false,
        }
    }

    pub fn mark_pending(&mut self, page: usize, scale: f32) {
        self.pending.insert(
            Key {
                page,
                scale: scale_key(scale),
            },
            Instant::now(),
        );
    }

    /// The worker dropped this request (superseded by a newer one for the same
    /// page). Clearing the flag lets the next frame ask again if the page is
    /// still on screen; without this the page would stay on a fallback texture
    /// forever.
    pub fn clear_pending(&mut self, page: usize, scale: f32) {
        self.pending.remove(&Key {
            page,
            scale: scale_key(scale),
        });
    }

    pub fn insert(
        &mut self,
        ctx: &egui::Context,
        page: usize,
        scale: f32,
        image: egui::ColorImage,
    ) {
        let key = Key {
            page,
            scale: scale_key(scale),
        };
        self.pending.remove(&key);
        let bytes = image.width() * image.height() * 4;
        let tex = ctx.load_texture(
            format!("page-{}-{}", key.page, key.scale),
            image,
            egui::TextureOptions::LINEAR,
        );
        if let Some(old) = self.map.insert(key, Entry { tex, bytes }) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        self.touch(key);
        self.evict();
    }

    /// How much texture memory this cache is holding. A budget is only worth
    /// having if something checks it, so the perf harness scrolls a
    /// thousand-page document past and watches this number. Nothing in the
    /// running app asks, which is why it is not compiled into one.
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many textures are held. Read with [`Self::bytes`]: together they
    /// say whether eviction is happening or the cache is merely small.
    #[cfg(test)]
    pub fn texture_count(&self) -> usize {
        self.map.len()
    }

    fn evict(&mut self) {
        while self.bytes > self.budget {
            // Least-recently-used first, but never something painted this
            // frame — dropping that would blank a visible page.
            let Some(pos) = self
                .lru
                .iter()
                .position(|k| !self.frame_touched.contains(k))
            else {
                break;
            };
            let key = self.lru.remove(pos);
            if let Some(entry) = self.map.remove(&key) {
                self.bytes -= entry.bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: usize, h: usize) -> egui::ColorImage {
        egui::ColorImage::filled([w, h], egui::Color32::WHITE)
    }

    /// One 100x100 texture is 40 000 bytes.
    const PX: usize = 100;
    const TEX_BYTES: usize = PX * PX * 4;

    #[test]
    fn pending_is_set_and_cleared_by_insert() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::default();
        assert!(!cache.is_pending(0, 1.0));
        cache.mark_pending(0, 1.0);
        assert!(cache.is_pending(0, 1.0));
        cache.insert(&ctx, 0, 1.0, image(PX, PX));
        assert!(!cache.is_pending(0, 1.0));
    }

    #[test]
    fn dropped_request_clears_pending_so_it_can_be_asked_again() {
        let mut cache = TextureCache::default();
        cache.mark_pending(3, 2.0);
        assert!(cache.is_pending(3, 2.0));
        cache.clear_pending(3, 2.0);
        assert!(!cache.is_pending(3, 2.0));
    }

    #[test]
    fn pending_is_per_scale() {
        let mut cache = TextureCache::default();
        cache.mark_pending(1, 0.22);
        assert!(cache.is_pending(1, 0.22));
        assert!(!cache.is_pending(1, 2.0));
    }

    #[test]
    fn eviction_respects_the_byte_budget() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::with_budget(TEX_BYTES * 3);
        for page in 0..5 {
            cache.begin_frame();
            cache.insert(&ctx, page, 1.0, image(PX, PX));
        }
        assert!(cache.bytes <= TEX_BYTES * 3, "over budget: {}", cache.bytes);
        assert_eq!(cache.map.len(), 3);
        // The oldest went first.
        assert!(cache.get(0, 1.0).is_none());
        assert!(cache.get(4, 1.0).is_some());
    }

    #[test]
    fn a_big_texture_evicts_more_small_ones() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::with_budget(TEX_BYTES * 4);
        for page in 0..4 {
            cache.begin_frame();
            cache.insert(&ctx, page, 1.0, image(PX, PX));
        }
        cache.begin_frame();
        cache.insert(&ctx, 9, 4.0, image(PX * 2, PX * 2)); // 4x the bytes
        assert!(cache.bytes <= TEX_BYTES * 4);
        assert!(cache.get(9, 4.0).is_some());
    }

    #[test]
    fn textures_painted_this_frame_survive_eviction() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::with_budget(TEX_BYTES);
        cache.begin_frame();
        cache.insert(&ctx, 0, 1.0, image(PX, PX));
        // Page 0 is on screen this frame; inserting page 1 must not blank it,
        // even though that puts the cache over budget.
        assert!(cache.get(0, 1.0).is_some());
        cache.insert(&ctx, 1, 1.0, image(PX, PX));
        assert!(cache.get(0, 1.0).is_some());
        // Next frame, with nothing painted yet, it becomes evictable again.
        cache.begin_frame();
        cache.insert(&ctx, 2, 1.0, image(PX, PX));
        assert!(cache.get(0, 1.0).is_none());
    }

    #[test]
    fn get_refreshes_the_lru() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::with_budget(TEX_BYTES * 2);
        cache.begin_frame();
        cache.insert(&ctx, 0, 1.0, image(PX, PX));
        cache.begin_frame();
        cache.insert(&ctx, 1, 1.0, image(PX, PX));
        cache.begin_frame();
        assert!(cache.get(0, 1.0).is_some()); // page 0 is now most recent
        cache.begin_frame();
        cache.insert(&ctx, 2, 1.0, image(PX, PX));
        assert!(cache.get(0, 1.0).is_some());
        assert!(cache.get(1, 1.0).is_none());
    }

    #[test]
    fn best_effort_prefers_the_sharpest_and_refreshes_the_lru() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::default();
        cache.insert(&ctx, 0, 0.5, image(PX, PX));
        cache.insert(&ctx, 0, 2.0, image(PX, PX));
        cache.insert(&ctx, 1, 1.0, image(PX, PX));
        let (scale, _) = cache.best_effort(0).expect("a texture for page 0");
        assert_eq!(scale, 2.0);
        // ...and it is now the most recently used key.
        assert_eq!(cache.lru.last().copied().map(|k| k.scale), Some(200));
    }

    #[test]
    fn best_effort_ignores_other_pages() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::default();
        cache.insert(&ctx, 1, 1.0, image(PX, PX));
        assert!(cache.best_effort(0).is_none());
    }

    #[test]
    fn replacing_a_key_does_not_double_count_bytes() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::default();
        cache.insert(&ctx, 0, 1.0, image(PX, PX));
        cache.insert(&ctx, 0, 1.0, image(PX, PX));
        assert_eq!(cache.bytes, TEX_BYTES);
        assert_eq!(cache.map.len(), 1);
    }
}
