//! A fast local browser for NASA Mars 2020 (Perseverance) raw images.
//!
//! The public raw-images feed behind
//! <https://mars.nasa.gov/mars2020/multimedia/raw-images/> is slow to browse
//! because every navigation re-fetches listings and full images. This crate
//! wraps that feed with a local SQLite metadata cache and an on-disk image
//! cache so repeat browsing is instant and works offline.
//!
//! The feed itself is undocumented and unversioned; its `type` field (e.g.
//! `mars2020-images-list-1.1`) is the only version signal.

pub mod app;
pub mod cache;
pub mod client;
pub mod fetch;
pub mod model;
pub mod query;
pub mod textures;
pub mod viewer;

pub use app::App;
pub use cache::Cache;
pub use client::Client;
pub use fetch::{Fetcher, ImageKind, Update};
pub use model::{Image, ImageListResponse, ImageSize};
pub use query::{MARS2020_CAMERAS, MAX_PAGE_SIZE, Order, Query};
pub use textures::{TextureStore, Tier};
