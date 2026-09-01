//! Local caches that make browsing fast and offline-capable.
//!
//! Two layers share one SQLite database:
//!
//! * a *metadata* cache of image records and of the page listings that
//!   produced them, so a repeat browse costs no network at all;
//! * an *index* for the on-disk blob cache, tracking size and last access so
//!   the store can be held under a byte budget.
//!
//! Image bytes live in files rather than in SQLite: they are large, immutable
//! and content-addressed, and keeping them out of the database keeps the
//! database small enough to stay fast.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::model::Image;
use crate::query::Query;

/// Default ceiling for the on-disk image cache.
pub const DEFAULT_CACHE_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// How long a cached listing page is considered current.
///
/// New images land continuously, so a listing is refreshed periodically even
/// when cached. Individual image records are immutable and never expire.
pub const LISTING_TTL_SECS: i64 = 15 * 60;

pub struct Cache {
    db: Connection,
    blob_dir: PathBuf,
    budget: u64,
}

impl Cache {
    /// Open (creating if needed) the cache under the platform cache directory.
    pub fn open_default() -> Result<Self> {
        let dir = default_cache_dir()?;
        Self::open_at(&dir, DEFAULT_CACHE_BUDGET)
    }

    pub fn open_at(dir: &Path, budget: u64) -> Result<Self> {
        let blob_dir = dir.join("images");
        std::fs::create_dir_all(&blob_dir)
            .with_context(|| format!("creating cache directory {}", blob_dir.display()))?;

        let db = Connection::open(dir.join("metadata.sqlite"))
            .with_context(|| format!("opening cache database in {}", dir.display()))?;

        // WAL keeps reads from blocking the writer, which matters because the
        // UI reads while background fetches write.
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;

        let cache = Self {
            db,
            blob_dir,
            budget,
        };
        cache.migrate()?;
        Ok(cache)
    }

    fn migrate(&self) -> Result<()> {
        self.db
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS images (
                     imageid    TEXT PRIMARY KEY,
                     sol        INTEGER,
                     instrument TEXT,
                     taken_utc  TEXT,
                     payload    TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS images_sol ON images(sol);

                 CREATE TABLE IF NOT EXISTS listings (
                     query_key     TEXT NOT NULL,
                     page          INTEGER NOT NULL,
                     total_results INTEGER,
                     fetched_at    INTEGER NOT NULL,
                     imageids      TEXT NOT NULL,
                     PRIMARY KEY (query_key, page)
                 );

                 CREATE TABLE IF NOT EXISTS blobs (
                     url        TEXT PRIMARY KEY,
                     path       TEXT NOT NULL,
                     bytes      INTEGER NOT NULL,
                     accessed_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS blobs_accessed ON blobs(accessed_at);",
            )
            .context("creating cache schema")?;
        Ok(())
    }

    // -- metadata -----------------------------------------------------------

    /// Store a listing page and the image records it contains.
    pub fn put_listing(
        &mut self,
        query: &Query,
        page: u64,
        total_results: Option<u64>,
        images: &[Image],
    ) -> Result<()> {
        let key = query.cache_key();
        let ids: Vec<&str> = images.iter().map(|i| i.id()).collect();
        let tx = self.db.transaction()?;

        for image in images {
            tx.execute(
                "INSERT INTO images (imageid, sol, instrument, taken_utc, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(imageid) DO UPDATE SET payload = excluded.payload",
                params![
                    image.id(),
                    image.sol,
                    image.camera.instrument.as_deref(),
                    image.date_taken_utc.as_deref(),
                    serde_json::to_string(image)?,
                ],
            )?;
        }

        tx.execute(
            "INSERT INTO listings (query_key, page, total_results, fetched_at, imageids)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(query_key, page) DO UPDATE SET
                 total_results = excluded.total_results,
                 fetched_at    = excluded.fetched_at,
                 imageids      = excluded.imageids",
            params![
                key,
                page as i64,
                total_results.map(|t| t as i64),
                now_secs(),
                serde_json::to_string(&ids)?,
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Read back a cached listing page, if present.
    ///
    /// `allow_stale` returns entries past [`LISTING_TTL_SECS`], which is what
    /// makes the app usable while offline.
    pub fn listing(&self, query: &Query, page: u64, allow_stale: bool) -> Result<Option<Listing>> {
        let row = self
            .db
            .query_row(
                "SELECT total_results, fetched_at, imageids FROM listings
                 WHERE query_key = ?1 AND page = ?2",
                params![query.cache_key(), page as i64],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;

        let Some((total, fetched_at, ids_json)) = row else {
            return Ok(None);
        };

        let stale = now_secs() - fetched_at > LISTING_TTL_SECS;
        if stale && !allow_stale {
            return Ok(None);
        }

        let ids: Vec<String> = serde_json::from_str(&ids_json)?;
        let mut images = Vec::with_capacity(ids.len());
        for id in &ids {
            // A record can be missing only if the database was edited
            // externally; skip rather than fail the whole page.
            if let Some(image) = self.image(id)? {
                images.push(image);
            }
        }

        Ok(Some(Listing {
            images,
            total_results: total.map(|t| t as u64),
            stale,
        }))
    }

    pub fn image(&self, imageid: &str) -> Result<Option<Image>> {
        let payload = self
            .db
            .query_row(
                "SELECT payload FROM images WHERE imageid = ?1",
                params![imageid],
                |r| r.get::<_, String>(0),
            )
            .optional()?;

        Ok(match payload {
            Some(json) => serde_json::from_str(&json).ok(),
            None => None,
        })
    }

    pub fn put_image(&self, image: &Image) -> Result<()> {
        self.db.execute(
            "INSERT INTO images (imageid, sol, instrument, taken_utc, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(imageid) DO UPDATE SET payload = excluded.payload",
            params![
                image.id(),
                image.sol,
                image.camera.instrument.as_deref(),
                image.date_taken_utc.as_deref(),
                serde_json::to_string(image)?,
            ],
        )?;
        Ok(())
    }

    // -- blobs --------------------------------------------------------------

    /// Look up cached bytes for `url`, refreshing its last-access time.
    pub fn blob(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let path = self
            .db
            .query_row("SELECT path FROM blobs WHERE url = ?1", params![url], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;

        let Some(path) = path else {
            return Ok(None);
        };

        match std::fs::read(&path) {
            Ok(bytes) => {
                let accessed_at = self.next_access()?;
                self.db.execute(
                    "UPDATE blobs SET accessed_at = ?2 WHERE url = ?1",
                    params![url, accessed_at],
                )?;
                Ok(Some(bytes))
            }
            // The file was removed behind our back; drop the stale index row
            // so the caller simply refetches.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.db
                    .execute("DELETE FROM blobs WHERE url = ?1", params![url])?;
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("reading cached blob {path}")),
        }
    }

    /// Store bytes for `url` and evict if the budget is exceeded.
    pub fn put_blob(&self, url: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.blob_dir.join(blob_name(url));
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing cached blob {}", path.display()))?;

        let accessed_at = self.next_access()?;
        self.db.execute(
            "INSERT INTO blobs (url, path, bytes, accessed_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(url) DO UPDATE SET
                 path = excluded.path,
                 bytes = excluded.bytes,
                 accessed_at = excluded.accessed_at",
            params![url, path.to_string_lossy(), bytes.len() as i64, accessed_at],
        )?;

        self.evict_to_budget()?;
        Ok(path)
    }

    /// Remove cached bytes and their index entry.
    pub fn remove_blob(&self, url: &str) -> Result<bool> {
        let path = self
            .db
            .query_row("SELECT path FROM blobs WHERE url = ?1", params![url], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;

        let Some(path) = path else {
            return Ok(false);
        };

        remove_blob_file(Path::new(&path))?;
        self.db
            .execute("DELETE FROM blobs WHERE url = ?1", params![url])?;
        Ok(true)
    }

    /// Total bytes tracked in the blob cache.
    pub fn blob_bytes(&self) -> Result<u64> {
        let total: i64 =
            self.db
                .query_row("SELECT COALESCE(SUM(bytes), 0) FROM blobs", [], |r| {
                    r.get(0)
                })?;
        Ok(total as u64)
    }

    /// Drop least-recently-used blobs until the cache fits its budget.
    pub fn evict_to_budget(&self) -> Result<u64> {
        let mut total = self.blob_bytes()?;
        if total <= self.budget {
            return Ok(0);
        }

        let mut stmt = self
            .db
            .prepare("SELECT url, path, bytes FROM blobs ORDER BY accessed_at ASC")?;
        let victims = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut freed = 0;
        for (url, path, bytes) in victims {
            if total <= self.budget {
                break;
            }
            remove_blob_file(Path::new(&path))?;
            self.db
                .execute("DELETE FROM blobs WHERE url = ?1", params![url])?;
            total = total.saturating_sub(bytes);
            freed += bytes;
        }

        Ok(freed)
    }

    /// A logical clock gives every access a strict order. Wall-clock seconds
    /// make a burst of downloads tie, at which point eviction is no longer LRU.
    fn next_access(&self) -> Result<i64> {
        self.db
            .query_row(
                "SELECT COALESCE(MAX(accessed_at), 0) + 1 FROM blobs",
                [],
                |r| r.get(0),
            )
            .context("advancing the blob access clock")
    }
}

pub struct Listing {
    pub images: Vec<Image>,
    pub total_results: Option<u64>,
    /// True when served past its TTL, i.e. offline or not yet refreshed.
    pub stale: bool,
}

pub fn default_cache_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "npv", "nasa-photo-viewer")
        .context("locating the platform cache directory")?;
    Ok(dirs.cache_dir().to_path_buf())
}

/// Content-addressed file name, keeping the source extension so the files
/// remain openable by hand.
fn blob_name(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    let hash = digest.iter().take(16).fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    let ext = url
        .rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, e)| e)
        .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("img");

    format!("{hash}.{ext}")
}

fn remove_blob_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        // The index row still has to go if the file was removed externally.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing cached blob {}", path.display())),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "npv-test-{tag}-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn image(id: &str, sol: i64) -> Image {
        serde_json::from_value(serde_json::json!({
            "imageid": id,
            "sol": sol,
            "camera": { "instrument": "NAVCAM_LEFT" },
            "image_files": { "large": format!("https://x/{id}_1200.jpg") },
        }))
        .unwrap()
    }

    #[test]
    fn round_trips_a_listing_without_the_network() {
        let dir = temp_dir("listing");
        let mut cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let query = Query::default();
        let images = vec![image("A", 10), image("B", 10)];

        cache.put_listing(&query, 0, Some(2), &images).unwrap();
        let got = cache.listing(&query, 0, false).unwrap().unwrap();

        assert_eq!(got.total_results, Some(2));
        assert!(!got.stale);
        assert_eq!(
            got.images
                .iter()
                .map(|i| i.id().to_string())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn distinguishes_listings_by_query() {
        let dir = temp_dir("querykey");
        let mut cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();

        let a = Query {
            min_sol: Some(10),
            ..Query::default()
        };
        let b = Query {
            min_sol: Some(20),
            ..Query::default()
        };
        cache
            .put_listing(&a, 0, Some(1), &[image("A", 10)])
            .unwrap();

        assert!(cache.listing(&a, 0, false).unwrap().is_some());
        // A different filter must not read another filter's page.
        assert!(cache.listing(&b, 0, false).unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn serves_stale_listings_only_when_asked() {
        let dir = temp_dir("stale");
        let mut cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let query = Query::default();
        cache
            .put_listing(&query, 0, Some(1), &[image("A", 1)])
            .unwrap();

        // Backdate the entry well past its TTL.
        cache
            .db
            .execute(
                "UPDATE listings SET fetched_at = ?1",
                params![now_secs() - LISTING_TTL_SECS - 60],
            )
            .unwrap();

        assert!(cache.listing(&query, 0, false).unwrap().is_none());
        let stale = cache.listing(&query, 0, true).unwrap().unwrap();
        assert!(stale.stale, "offline reads must be flagged as stale");
        assert_eq!(stale.images.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trips_blobs() {
        let dir = temp_dir("blob");
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();

        assert!(cache.blob("https://x/a.jpg").unwrap().is_none());
        cache.put_blob("https://x/a.jpg", b"hello").unwrap();
        assert_eq!(cache.blob("https://x/a.jpg").unwrap().unwrap(), b"hello");
        assert_eq!(cache.blob_bytes().unwrap(), 5);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refetches_when_a_cached_file_disappears() {
        let dir = temp_dir("missing");
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let path = cache.put_blob("https://x/a.jpg", b"hello").unwrap();

        std::fs::remove_file(&path).unwrap();

        // Must report a miss rather than erroring, and forget the index row.
        assert!(cache.blob("https://x/a.jpg").unwrap().is_none());
        assert_eq!(cache.blob_bytes().unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removing_a_blob_drops_its_file_and_index_entry() {
        let dir = temp_dir("remove");
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let url = "https://x/a.jpg";
        let path = cache.put_blob(url, b"hello").unwrap();

        assert!(cache.remove_blob(url).unwrap());
        assert!(!path.exists());
        assert_eq!(cache.blob_bytes().unwrap(), 0);
        assert!(
            !cache.remove_blob(url).unwrap(),
            "an absent blob is a no-op"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn evicts_least_recently_used_blobs_over_budget() {
        let dir = temp_dir("evict");
        let cache = Cache::open_at(&dir, 100).unwrap();

        cache.put_blob("https://x/old.jpg", &[0u8; 60]).unwrap();
        cache.put_blob("https://x/mid.jpg", &[0u8; 30]).unwrap();
        // Touch the oldest so recency, not insertion order, decides.
        assert!(cache.blob("https://x/old.jpg").unwrap().is_some());

        cache.put_blob("https://x/new.jpg", &[0u8; 30]).unwrap();

        assert!(cache.blob_bytes().unwrap() <= 100);
        assert!(cache.blob("https://x/old.jpg").unwrap().is_some());
        assert!(cache.blob("https://x/mid.jpg").unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn survives_reopening() {
        let dir = temp_dir("reopen");
        {
            let mut cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
            cache
                .put_listing(&Query::default(), 0, Some(1), &[image("A", 7)])
                .unwrap();
        }

        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let got = cache.listing(&Query::default(), 0, true).unwrap().unwrap();
        assert_eq!(got.images[0].sol, Some(7));

        std::fs::remove_dir_all(&dir).ok();
    }
}
