//! A size-bounded store of GPU textures.
//!
//! Decoded images are held on the GPU, so keeping every one ever seen is not
//! an option: a few hundred thumbnails plus a handful of full-size renditions
//! runs to hundreds of megabytes. Entries are therefore evicted
//! least-recently-used, with separate budgets per tier so that a burst of
//! large images cannot flush the thumbnail grid.

use std::collections::HashMap;

use egui::TextureHandle;

/// Which budget an entry is charged against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Gallery thumbnails: small, and wanted again as soon as the user
    /// scrolls back.
    Thumbnail,
    /// Detail renditions: large, and only a few are useful at a time.
    Detail,
}

/// Thumbnails to keep. Enough to cover a long scroll without re-decoding.
pub const DEFAULT_THUMBNAIL_CAPACITY: usize = 400;

/// Detail renditions to keep. Must exceed the prefetch window either side of
/// the selection, or stepping through images would evict what was just
/// fetched.
pub const DEFAULT_DETAIL_CAPACITY: usize = 12;

struct Entry {
    handle: TextureHandle,
    tier: Tier,
    used: u64,
}

pub struct TextureStore {
    entries: HashMap<String, Entry>,
    thumbnail_capacity: usize,
    detail_capacity: usize,
    clock: u64,
}

impl Default for TextureStore {
    fn default() -> Self {
        Self::new(DEFAULT_THUMBNAIL_CAPACITY, DEFAULT_DETAIL_CAPACITY)
    }
}

impl TextureStore {
    pub fn new(thumbnail_capacity: usize, detail_capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            thumbnail_capacity: thumbnail_capacity.max(1),
            detail_capacity: detail_capacity.max(1),
            clock: 0,
        }
    }

    /// Fetch a texture, marking it as most recently used.
    pub fn get(&mut self, url: &str) -> Option<TextureHandle> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(url)?;
        entry.used = clock;
        Some(entry.handle.clone())
    }

    /// Whether a texture is held, without affecting eviction order.
    ///
    /// Used to decide whether a fetch is needed; counting that as a use would
    /// keep entries alive that nothing actually displays.
    pub fn contains(&self, url: &str) -> bool {
        self.entries.contains_key(url)
    }

    pub fn insert(&mut self, url: String, handle: TextureHandle, tier: Tier) {
        self.clock += 1;
        self.entries.insert(
            url,
            Entry {
                handle,
                tier,
                used: self.clock,
            },
        );
        self.evict(tier);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn count(&self, tier: Tier) -> usize {
        self.entries.values().filter(|e| e.tier == tier).count()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn capacity(&self, tier: Tier) -> usize {
        match tier {
            Tier::Thumbnail => self.thumbnail_capacity,
            Tier::Detail => self.detail_capacity,
        }
    }

    fn evict(&mut self, tier: Tier) {
        let capacity = self.capacity(tier);
        while self.count(tier) > capacity {
            let victim = self
                .entries
                .iter()
                .filter(|(_, e)| e.tier == tier)
                .min_by_key(|(_, e)| e.used)
                .map(|(url, _)| url.clone());

            match victim {
                Some(url) => {
                    self.entries.remove(&url);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear();
        ctx
    }

    fn handle(ctx: &egui::Context, name: &str) -> TextureHandle {
        ctx.load_texture(
            name,
            egui::ColorImage::from_rgba_unmultiplied([1, 1], &[1, 2, 3, 255]),
            egui::TextureOptions::LINEAR,
        )
    }

    #[test]
    fn stores_and_returns_textures() {
        let ctx = ctx();
        let mut store = TextureStore::default();

        assert!(store.is_empty());
        store.insert("a".into(), handle(&ctx, "a"), Tier::Thumbnail);

        assert!(store.contains("a"));
        assert!(store.get("a").is_some());
        assert!(store.get("missing").is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn evicts_the_least_recently_used_entry_of_a_tier() {
        let ctx = ctx();
        let mut store = TextureStore::new(8, 2);

        store.insert("one".into(), handle(&ctx, "1"), Tier::Detail);
        store.insert("two".into(), handle(&ctx, "2"), Tier::Detail);
        // Touch the oldest so recency, not insertion order, decides.
        store.get("one");

        store.insert("three".into(), handle(&ctx, "3"), Tier::Detail);

        assert_eq!(store.count(Tier::Detail), 2);
        assert!(store.contains("one"), "recently used entry was evicted");
        assert!(!store.contains("two"));
        assert!(store.contains("three"));
    }

    #[test]
    fn tiers_have_independent_budgets() {
        let ctx = ctx();
        let mut store = TextureStore::new(3, 1);

        for i in 0..3 {
            store.insert(format!("t{i}"), handle(&ctx, "t"), Tier::Thumbnail);
        }
        // A burst of large images must not flush the thumbnail grid.
        for i in 0..5 {
            store.insert(format!("d{i}"), handle(&ctx, "d"), Tier::Detail);
        }

        assert_eq!(store.count(Tier::Thumbnail), 3);
        assert_eq!(store.count(Tier::Detail), 1);
    }

    #[test]
    fn containment_check_does_not_count_as_use() {
        let ctx = ctx();
        let mut store = TextureStore::new(8, 2);

        store.insert("old".into(), handle(&ctx, "o"), Tier::Detail);
        store.insert("new".into(), handle(&ctx, "n"), Tier::Detail);

        // Merely asking whether a fetch is needed must not keep it alive.
        assert!(store.contains("old"));
        store.insert("newest".into(), handle(&ctx, "x"), Tier::Detail);

        assert!(!store.contains("old"));
    }

    #[test]
    fn reinserting_refreshes_recency_without_growing() {
        let ctx = ctx();
        let mut store = TextureStore::new(8, 2);

        store.insert("a".into(), handle(&ctx, "a"), Tier::Detail);
        store.insert("b".into(), handle(&ctx, "b"), Tier::Detail);
        store.insert("a".into(), handle(&ctx, "a2"), Tier::Detail);
        store.insert("c".into(), handle(&ctx, "c"), Tier::Detail);

        assert_eq!(store.count(Tier::Detail), 2);
        assert!(store.contains("a"));
        assert!(store.contains("c"));
        assert!(!store.contains("b"), "the stalest entry should have gone");
    }

    #[test]
    fn a_zero_capacity_still_keeps_one_entry() {
        let ctx = ctx();
        let mut store = TextureStore::new(0, 0);

        store.insert("a".into(), handle(&ctx, "a"), Tier::Detail);
        assert!(store.contains("a"));
    }

    #[test]
    fn clearing_drops_everything() {
        let ctx = ctx();
        let mut store = TextureStore::default();
        store.insert("a".into(), handle(&ctx, "a"), Tier::Thumbnail);

        store.clear();
        assert!(store.is_empty());
    }
}
