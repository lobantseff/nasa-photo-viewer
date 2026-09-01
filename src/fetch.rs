//! Background fetching that keeps the UI thread free.
//!
//! egui repaints on the main thread, so every network and disk access happens
//! on a Tokio runtime and results arrive through a channel that the UI drains
//! once per frame. Image decoding also happens off-thread: the UI receives a
//! ready-to-upload [`egui::ColorImage`] rather than raw bytes.
//!
//! Requests are de-duplicated by key. Scrolling a gallery repeatedly asks for
//! the same rows, and without dedupe that would multiply into redundant
//! requests against NASA's servers.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use tokio::runtime::Runtime;

use crate::cache::Cache;
use crate::client::Client;
use crate::model::Image;
use crate::query::Query;

/// Which rendition a pending image request refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKind {
    /// Gallery thumbnail.
    Thumbnail,
    /// The rendition shown in the detail view.
    Detail,
    /// The full-resolution original.
    Full,
}

/// A message from a background task to the UI.
pub enum Update {
    Listing {
        query_key: String,
        page: u64,
        images: Vec<Image>,
        total_results: Option<u64>,
        /// Served from cache because the network was unavailable.
        from_stale_cache: bool,
    },
    Image {
        url: String,
        kind: ImageKind,
        image: Box<egui::ColorImage>,
    },
    /// A request failed; the UI surfaces this without treating it as fatal.
    Failed { key: String, error: String },
    /// Network reachability changed.
    Connectivity { online: bool },
    /// GitHub reported its latest release. Only sent when the check succeeds:
    /// being unable to reach it is the normal offline case, not news.
    LatestRelease(crate::update::LatestRelease),
}

/// A listing served from the local cache.
pub struct CachedListing {
    pub images: Vec<Image>,
    pub total_results: Option<u64>,
    /// Past its refresh window, so a background refresh is warranted.
    pub stale: bool,
}

pub struct Fetcher {
    rt: Runtime,
    client: Arc<Client>,
    cache: Arc<Mutex<Cache>>,
    tx: Sender<Update>,
    rx: Receiver<Update>,
    inflight: HashSet<String>,
    /// Requests actually dispatched, never decremented. `inflight` drains as
    /// work finishes, so it cannot answer "was this ever asked for".
    issued: u64,
    online: bool,
    ctx: egui::Context,
}

impl Fetcher {
    pub fn new(ctx: egui::Context, cache: Cache) -> Result<Self> {
        Self::with_client(ctx, cache, Client::new()?)
    }

    /// Build with a specific [`Client`], used by tests to point at an
    /// unreachable endpoint and exercise the offline path.
    pub fn with_client(ctx: egui::Context, cache: Cache, client: Client) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            // A small pool is plenty: these tasks are I/O bound, and a wide
            // pool would only invite hammering the upstream service.
            .worker_threads(4)
            .enable_all()
            .build()
            .context("starting the background runtime")?;

        let (tx, rx) = channel();
        Ok(Self {
            rt,
            client: Arc::new(client),
            cache: Arc::new(Mutex::new(cache)),
            tx,
            rx,
            inflight: HashSet::new(),
            issued: 0,
            online: true,
            ctx,
        })
    }

    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn cache(&self) -> Arc<Mutex<Cache>> {
        Arc::clone(&self.cache)
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight.len()
    }

    /// Listing pages currently being fetched.
    pub fn inflight_listings(&self) -> usize {
        self.inflight
            .iter()
            .filter(|k| k.starts_with("listing:"))
            .count()
    }

    /// Images currently being fetched or decoded.
    pub fn inflight_images(&self) -> usize {
        self.inflight
            .iter()
            .filter(|k| k.starts_with("image:"))
            .count()
    }

    /// Total requests dispatched since start.
    pub fn issued_count(&self) -> u64 {
        self.issued
    }

    /// Read a listing page from cache, however old, reporting whether it is
    /// past its refresh window.
    ///
    /// Stale records are still perfectly good photographs. Withholding them
    /// buys nothing and costs a blank screen for as long as the upstream
    /// request takes, which for a cold query is many seconds.
    pub fn cached_listing(&self, query: &Query, page: u64) -> Option<CachedListing> {
        let cache = self.cache.lock().ok()?;
        let listing = cache.listing(query, page, true).ok().flatten()?;
        Some(CachedListing {
            images: listing.images,
            total_results: listing.total_results,
            stale: listing.stale,
        })
    }

    /// Request a listing page unless an identical request is already running.
    pub fn request_listing(&mut self, query: &Query, page: u64) {
        let key = format!("listing:{}:{page}", query.cache_key());
        if !self.inflight.insert(key.clone()) {
            return;
        }
        self.issued += 1;

        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (client, cache) = (Arc::clone(&self.client), Arc::clone(&self.cache));
        let (query, query_key) = (query.with_page(page), query.cache_key());

        self.rt.spawn(async move {
            let update = match client.list(&query).await {
                Ok(resp) => {
                    let images = resp.images;
                    if let Ok(mut cache) = cache.lock() {
                        let _ = cache.put_listing(&query, page, resp.total_results, &images);
                    }
                    let _ = tx.send(Update::Connectivity { online: true });
                    Update::Listing {
                        query_key,
                        page,
                        images,
                        total_results: resp.total_results,
                        from_stale_cache: false,
                    }
                }
                Err(err) => {
                    let _ = tx.send(Update::Connectivity { online: false });
                    // Fall back to whatever we already hold, however old, so
                    // the app stays usable without a network.
                    let cached = cache
                        .lock()
                        .ok()
                        .and_then(|c| c.listing(&query, page, true).ok().flatten());

                    match cached {
                        Some(listing) => Update::Listing {
                            query_key,
                            page,
                            images: listing.images,
                            total_results: listing.total_results,
                            from_stale_cache: true,
                        },
                        None => Update::Failed {
                            key: format!("listing:{query_key}:{page}"),
                            error: err.to_string(),
                        },
                    }
                }
            };

            let _ = tx.send(update);
            ctx.request_repaint();
        });
    }

    /// Request and decode an image, preferring cached bytes.
    pub fn request_image(&mut self, url: &str, kind: ImageKind) {
        let key = format!("image:{url}");
        if !self.inflight.insert(key.clone()) {
            return;
        }
        self.issued += 1;

        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (client, cache) = (Arc::clone(&self.client), Arc::clone(&self.cache));
        let url = url.to_string();

        self.rt.spawn(async move {
            let cached = cache.lock().ok().and_then(|c| c.blob(&url).ok().flatten());

            let bytes = match cached {
                Some(bytes) => Some(bytes),
                None => match client.fetch_bytes(&url).await {
                    Ok(bytes) => {
                        if let Ok(cache) = cache.lock() {
                            let _ = cache.put_blob(&url, &bytes);
                        }
                        let _ = tx.send(Update::Connectivity { online: true });
                        Some(bytes)
                    }
                    Err(err) => {
                        let _ = tx.send(Update::Connectivity { online: false });
                        let _ = tx.send(Update::Failed {
                            key: format!("image:{url}"),
                            error: err.to_string(),
                        });
                        None
                    }
                },
            };

            if let Some(bytes) = bytes {
                // Decode here rather than on the UI thread: a full-res PNG
                // takes long enough to drop frames.
                match decode(&bytes) {
                    Ok(image) => {
                        let _ = tx.send(Update::Image {
                            url,
                            kind,
                            image: Box::new(image),
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(Update::Failed {
                            key: format!("image:{url}"),
                            error: err.to_string(),
                        });
                    }
                }
            }

            ctx.request_repaint();
        });
    }

    /// Drain completed work. Call once per frame.
    pub fn poll(&mut self) -> Vec<Update> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Update::Connectivity { online }) => self.online = online,
                Ok(update) => {
                    match &update {
                        Update::Listing {
                            query_key, page, ..
                        } => {
                            self.inflight.remove(&format!("listing:{query_key}:{page}"));
                        }
                        Update::Image { url, .. } => {
                            self.inflight.remove(&format!("image:{url}"));
                        }
                        Update::Failed { key, .. } => {
                            self.inflight.remove(key);
                        }
                        Update::LatestRelease(_) => {
                            self.inflight.remove("release:latest");
                        }
                        Update::Connectivity { .. } => {}
                    }
                    out.push(update);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    /// Ask GitHub for the latest release, in the background.
    ///
    /// Deliberately quiet: a failure produces no message at all, since not
    /// reaching GitHub says nothing about whether an update exists, and it
    /// must not be mistaken for the image feed going offline.
    pub fn request_latest_release(&mut self) {
        let key = "release:latest".to_string();
        if !self.inflight.insert(key) {
            return;
        }
        self.issued += 1;

        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let client = Arc::clone(&self.client);

        self.rt.spawn(async move {
            if let Ok(bytes) = client.fetch_bytes(crate::update::LATEST_RELEASE_API).await
                && let Ok(latest) = serde_json::from_slice::<crate::update::LatestRelease>(&bytes)
            {
                let _ = tx.send(Update::LatestRelease(latest));
                ctx.request_repaint();
            }
        });
    }

    /// Copy a cached image to `dest`, fetching it first if necessary.
    pub fn save_image_to(&self, url: &str, dest: std::path::PathBuf) -> Result<()> {
        let cached = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?
            .blob(url)?;

        let bytes = match cached {
            Some(bytes) => bytes,
            None => {
                let client = Arc::clone(&self.client);
                let url = url.to_string();
                self.rt
                    .block_on(async move { client.fetch_bytes(&url).await })?
            }
        };

        std::fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
        Ok(())
    }
}

fn decode(bytes: &[u8]) -> Result<egui::ColorImage> {
    let decoded = image::load_from_memory(bytes).context("decoding image")?;
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cache::DEFAULT_CACHE_BUDGET;
    use crate::model::Image;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "npv-fetch-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Collect updates until one arrives or the deadline passes.
    fn wait_for_update(fetcher: &mut Fetcher) -> Vec<Update> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let updates = fetcher.poll();
            if !updates.is_empty() || std::time::Instant::now() > deadline {
                return updates;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn falls_back_to_cached_results_when_the_network_is_down() {
        let dir = temp_dir("offline");
        let query = Query::default();
        let image: Image = serde_json::from_value(serde_json::json!({
            "imageid": "CACHED_ONE",
            "sol": 1234,
            "camera": { "instrument": "NAVCAM_LEFT" },
        }))
        .unwrap();

        {
            let mut cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
            cache.put_listing(&query, 0, Some(1), &[image]).unwrap();
        }

        // Port 1 is reserved and refuses connections, so every request fails.
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let client = Client::with_endpoint("http://127.0.0.1:1/").unwrap();
        let mut fetcher = Fetcher::with_client(egui::Context::default(), cache, client).unwrap();

        fetcher.request_listing(&query, 0);
        let updates = wait_for_update(&mut fetcher);

        let listing = updates
            .into_iter()
            .find_map(|u| match u {
                Update::Listing {
                    images,
                    from_stale_cache,
                    ..
                } => Some((images, from_stale_cache)),
                _ => None,
            })
            .expect("offline request should still deliver cached results");

        assert!(listing.1, "result must be flagged as coming from cache");
        assert_eq!(listing.0[0].id(), "CACHED_ONE");
        assert!(
            !fetcher.is_online(),
            "connectivity should be reported as down"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_failure_when_offline_with_an_empty_cache() {
        let dir = temp_dir("offline-empty");
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let client = Client::with_endpoint("http://127.0.0.1:1/").unwrap();
        let mut fetcher = Fetcher::with_client(egui::Context::default(), cache, client).unwrap();

        fetcher.request_listing(&Query::default(), 0);
        let updates = wait_for_update(&mut fetcher);

        assert!(
            updates.iter().any(|u| matches!(u, Update::Failed { .. })),
            "an uncached offline request must surface an error"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inflight_work_is_reported_by_kind() {
        let dir = temp_dir("kinds");
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let client = Client::with_endpoint("http://127.0.0.1:1/").unwrap();
        let mut fetcher = Fetcher::with_client(egui::Context::default(), cache, client).unwrap();

        fetcher.request_listing(&Query::default(), 0);
        fetcher.request_image("https://x/a.jpg", ImageKind::Thumbnail);
        fetcher.request_image("https://x/b.jpg", ImageKind::Thumbnail);

        // A single slow listing alongside two images must not be reported as
        // one indistinguishable count.
        assert_eq!(fetcher.inflight_listings(), 1);
        assert_eq!(fetcher.inflight_images(), 2);
        assert_eq!(fetcher.inflight_count(), 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_issued_counter_survives_requests_completing() {
        let dir = temp_dir("issued");
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let client = Client::with_endpoint("http://127.0.0.1:1/").unwrap();
        let mut fetcher = Fetcher::with_client(egui::Context::default(), cache, client).unwrap();

        assert_eq!(fetcher.issued_count(), 0);
        fetcher.request_listing(&Query::default(), 0);
        assert_eq!(fetcher.issued_count(), 1);

        // Draining the in-flight set must not erase the record.
        let _ = wait_for_update(&mut fetcher);
        assert_eq!(fetcher.inflight_count(), 0);
        assert_eq!(fetcher.issued_count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn identical_requests_are_deduplicated() {
        let dir = temp_dir("dedupe");
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let client = Client::with_endpoint("http://127.0.0.1:1/").unwrap();
        let mut fetcher = Fetcher::with_client(egui::Context::default(), cache, client).unwrap();

        // Scrolling repeatedly asks for the same rows; without dedupe this
        // would multiply into redundant upstream requests.
        for _ in 0..5 {
            fetcher.request_listing(&Query::default(), 0);
        }
        assert_eq!(fetcher.inflight_count(), 1);
    }

    #[test]
    fn decodes_a_png_into_a_color_image() {
        // 1x1 red PNG.
        let png = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d,
            0xb0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];

        let img = decode(&png).unwrap();
        assert_eq!(img.size, [1, 1]);
    }

    #[test]
    fn reports_an_error_for_undecodable_bytes() {
        assert!(decode(b"definitely not an image").is_err());
    }
}
