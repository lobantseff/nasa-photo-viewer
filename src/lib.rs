//! A typed client for the Mars 2020 (Perseverance) raw-images feed that backs
//! <https://mars.nasa.gov/mars2020/multimedia/raw-images/>.
//!
//! The feed is public and key-less but undocumented and unversioned; the
//! `type` field (e.g. `mars2020-images-list-1.1`) is the only version signal.

pub mod client;
pub mod model;
pub mod query;

pub use client::Client;
pub use model::{Image, ImageListResponse, ImageSize};
pub use query::{MARS2020_CAMERAS, MAX_PAGE_SIZE, Order, Query};
