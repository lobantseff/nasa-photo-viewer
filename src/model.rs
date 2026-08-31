//! Typed representations of the `mars.nasa.gov/rss/api` raw-image feed.
//!
//! The feed is undocumented and unversioned, and its field types are not
//! stable across missions and sols: numbers sometimes arrive as JSON strings,
//! and older sols omit whole blocks. Every field is therefore optional and
//! numeric fields accept both representations.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// A page of results from a `feed=raw_images` list query.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageListResponse {
    #[serde(default)]
    pub images: Vec<Image>,
    /// Present (with an empty `images` array and HTTP 200) when a page is past
    /// the end of the result set, e.g. `"No more images."`.
    #[serde(default)]
    pub error_message: Option<String>,
    /// Number of image records matching the query.
    #[serde(default, deserialize_with = "flexible_u64_opt")]
    pub total_results: Option<u64>,
    /// Number of underlying image *files*, which is larger than
    /// [`Self::total_results`]. Not interchangeable for pagination math.
    #[serde(default, deserialize_with = "flexible_u64_opt")]
    pub total_images: Option<u64>,
    #[serde(default, deserialize_with = "flexible_u64_opt")]
    pub page: Option<u64>,
    #[serde(default, deserialize_with = "flexible_u64_opt")]
    pub per_page: Option<u64>,
    /// Schema marker, e.g. `mars2020-images-list-1.1`. The only version signal.
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub mission: Option<String>,
}

impl ImageListResponse {
    /// True when the feed reports it has run out of results for this query.
    pub fn is_exhausted(&self) -> bool {
        self.images.is_empty()
    }
}

/// Response shape of a single-image lookup (`&id=...`).
///
/// Note the key is `image`, not `images` as in [`ImageListResponse`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageDetailResponse {
    #[serde(default)]
    pub image: Vec<Image>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Image {
    #[serde(default)]
    pub imageid: Option<String>,
    #[serde(default, deserialize_with = "flexible_i64_opt")]
    pub sol: Option<i64>,
    #[serde(default, deserialize_with = "flexible_i64_opt")]
    pub site: Option<i64>,
    #[serde(default, deserialize_with = "flexible_string_opt")]
    pub drive: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub credit: Option<String>,
    #[serde(default)]
    pub sample_type: Option<String>,
    #[serde(default)]
    pub date_taken_utc: Option<String>,
    #[serde(default)]
    pub date_taken_mars: Option<String>,
    #[serde(default)]
    pub date_received: Option<String>,
    /// Rover attitude quaternion, formatted as `(w,x,y,z)`.
    #[serde(default)]
    pub attitude: Option<String>,
    /// Canonical web page for this image.
    #[serde(default)]
    pub link: Option<String>,
    /// Canonical single-image API URL.
    #[serde(default)]
    pub json_link: Option<String>,
    #[serde(default)]
    pub image_files: ImageFiles,
    #[serde(default)]
    pub camera: Camera,
    #[serde(default)]
    pub extended: Extended,
}

impl Image {
    pub fn id(&self) -> &str {
        self.imageid.as_deref().unwrap_or("<unknown>")
    }

    /// Best available URL at or below `size`, falling back through smaller
    /// variants so that images lacking a given rendition still resolve.
    pub fn url_for(&self, size: ImageSize) -> Option<&str> {
        let f = &self.image_files;
        let ladder: &[&Option<String>] = match size {
            ImageSize::Small => &[&f.small, &f.medium, &f.large, &f.full_res],
            ImageSize::Medium => &[&f.medium, &f.large, &f.small, &f.full_res],
            ImageSize::Large => &[&f.large, &f.medium, &f.full_res, &f.small],
            ImageSize::FullRes => &[&f.full_res, &f.large, &f.medium, &f.small],
        };
        ladder.iter().find_map(|c| c.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSize {
    Small,
    Medium,
    Large,
    FullRes,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageFiles {
    #[serde(default)]
    pub small: Option<String>,
    #[serde(default)]
    pub medium: Option<String>,
    #[serde(default)]
    pub large: Option<String>,
    #[serde(default)]
    pub full_res: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Camera {
    #[serde(default)]
    pub instrument: Option<String>,
    #[serde(default)]
    pub filter_name: Option<String>,
    #[serde(default)]
    pub camera_vector: Option<String>,
    #[serde(default)]
    pub camera_position: Option<String>,
    #[serde(default)]
    pub camera_model_type: Option<String>,
    #[serde(default)]
    pub camera_model_component_list: Option<String>,
}

impl Camera {
    pub fn instrument_or_unknown(&self) -> &str {
        self.instrument.as_deref().unwrap_or("UNKNOWN")
    }
}

/// Instrument-specific metadata. Keys vary by camera and mission, so unknown
/// entries are preserved in [`Extended::other`] rather than dropped.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Extended {
    #[serde(default)]
    pub dimension: Option<String>,
    #[serde(default, rename = "subframeRect")]
    pub subframe_rect: Option<String>,
    #[serde(default, rename = "scaleFactor")]
    pub scale_factor: Option<String>,
    /// Spacecraft clock at exposure.
    #[serde(default)]
    pub sclk: Option<String>,
    #[serde(default, rename = "mastAz")]
    pub mast_az: Option<String>,
    #[serde(default, rename = "mastEl")]
    pub mast_el: Option<String>,
    #[serde(default)]
    pub xyz: Option<String>,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

fn flexible_u64_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(flexible_i64_opt(d)?.and_then(|v| u64::try_from(v).ok()))
}

fn flexible_i64_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}

fn flexible_string_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_list_page() {
        let raw = include_str!("../tests/fixtures/list_page.json");
        let resp: ImageListResponse = serde_json::from_str(raw).unwrap();

        assert_eq!(resp.total_results, Some(563603));
        assert_eq!(resp.total_images, Some(1029613));
        // per_page arrives as the JSON string "3", not a number.
        assert_eq!(resp.per_page, Some(3));
        assert_eq!(resp.page, Some(0));
        assert_eq!(resp.r#type.as_deref(), Some("mars2020-images-list-1.1"));
        assert!(!resp.is_exhausted());

        let img = &resp.images[0];
        assert_eq!(img.sol, Some(1965));
        assert_eq!(img.camera.instrument_or_unknown(), "NAVCAM_RIGHT");
        // `drive` is a quoted number in the feed.
        assert_eq!(img.drive.as_deref(), Some("806"));
        assert_eq!(img.extended.dimension.as_deref(), Some("(1288,968)"));
        assert!(img.url_for(ImageSize::FullRes).unwrap().ends_with(".png"));
    }

    #[test]
    fn detects_the_end_of_the_result_set() {
        // A page past the end still returns HTTP 200.
        let raw = r#"{"images":[],"per_page":"1","error_message":"No more images.",
                      "total_results":0,"type":"mars2020-images-list-1.1","page":9999}"#;
        let resp: ImageListResponse = serde_json::from_str(raw).unwrap();

        assert!(resp.is_exhausted());
        assert_eq!(resp.error_message.as_deref(), Some("No more images."));
    }

    #[test]
    fn tolerates_sparse_records() {
        let resp: ImageListResponse =
            serde_json::from_str(r#"{"images":[{"imageid":"X"}]}"#).unwrap();
        let img = &resp.images[0];

        assert_eq!(img.id(), "X");
        assert_eq!(img.sol, None);
        assert_eq!(img.url_for(ImageSize::Large), None);
        assert_eq!(img.camera.instrument_or_unknown(), "UNKNOWN");
    }

    #[test]
    fn falls_back_when_a_rendition_is_missing() {
        let img: Image = serde_json::from_str(
            r#"{"imageid":"X","image_files":{"small":"s.jpg","full_res":"f.png"}}"#,
        )
        .unwrap();

        assert_eq!(img.url_for(ImageSize::Large), Some("f.png"));
        assert_eq!(img.url_for(ImageSize::Small), Some("s.jpg"));
    }

    #[test]
    fn keeps_unknown_extended_keys() {
        let img: Image =
            serde_json::from_str(r#"{"imageid":"X","extended":{"someFutureKey":"7"}}"#).unwrap();

        assert_eq!(
            img.extended.other.get("someFutureKey").unwrap().as_str(),
            Some("7")
        );
    }

    #[test]
    fn single_image_lookup_uses_the_image_key() {
        let resp: ImageDetailResponse =
            serde_json::from_str(r#"{"image":[{"imageid":"NRF_1965","sol":1965}]}"#).unwrap();

        assert_eq!(resp.image[0].sol, Some(1965));
    }
}
