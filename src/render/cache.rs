//! GPU texture cache for rendered pages, keyed by (page, scale bucket).

use std::collections::HashMap;

use eframe::egui;

use super::scale_key;

const MAX_TEXTURES: usize = 14;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key {
    page: usize,
    scale: u32,
}

#[derive(Default)]
pub struct TextureCache {
    map: HashMap<Key, egui::TextureHandle>,
    /// Most-recently-used at the back.
    lru: Vec<Key>,
    /// Requests sent to the worker but not yet answered.
    pending: HashMap<Key, ()>,
}

impl TextureCache {
    pub fn get(&mut self, page: usize, scale: f32) -> Option<egui::TextureHandle> {
        let key = Key {
            page,
            scale: scale_key(scale),
        };
        let tex = self.map.get(&key).cloned()?;
        self.lru.retain(|k| *k != key);
        self.lru.push(key);
        Some(tex)
    }

    /// The best available texture for this page at any scale (used while the
    /// exact scale renders), together with its scale.
    pub fn best_effort(&self, page: usize) -> Option<(f32, egui::TextureHandle)> {
        self.map
            .iter()
            .filter(|(k, _)| k.page == page)
            .max_by_key(|(k, _)| k.scale)
            .map(|(k, t)| (k.scale as f32 / 100.0, t.clone()))
    }

    pub fn is_pending(&self, page: usize, scale: f32) -> bool {
        self.pending.contains_key(&Key {
            page,
            scale: scale_key(scale),
        })
    }

    pub fn mark_pending(&mut self, page: usize, scale: f32) {
        self.pending.insert(
            Key {
                page,
                scale: scale_key(scale),
            },
            (),
        );
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
        let tex = ctx.load_texture(
            format!("page-{}-{}", key.page, key.scale),
            image,
            egui::TextureOptions::LINEAR,
        );
        self.map.insert(key, tex);
        self.lru.retain(|k| *k != key);
        self.lru.push(key);

        while self.map.len() > MAX_TEXTURES {
            let evict = self.lru.remove(0);
            self.map.remove(&evict);
        }
    }
}
