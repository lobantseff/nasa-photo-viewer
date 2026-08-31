# nasa-photo-viewer

A fast desktop browser (egui) for Perseverance raw images from
<https://mars.nasa.gov/mars2020/multimedia/raw-images/>.

The NASA page is slow because every navigation re-fetches listings and
images. This app caches listing metadata in SQLite and image bytes on disk,
so repeat browsing is instant and works offline.

## Features

- Thumbnail gallery with infinite scroll and background prefetch
- Zoom (cursor-anchored) and drag-to-pan detail view
- 1200px preview loads immediately; full-resolution PNG on demand
- Filter by sol and camera; sort newest/oldest/by capture date
- Local caching: SQLite for metadata, disk for images, ~2 GiB LRU budget
- Works offline against anything previously browsed
- Remembers window size and last filters

## Running

```bash
cargo run --release
```

Controls in the detail view:

| Input | Action |
| --- | --- |
| Mouse wheel | Zoom about the pointer |
| Trackpad pinch, or `Ctrl`+scroll | Zoom about the pointer |
| Trackpad two-finger swipe, or drag | Pan a zoomed image |
| `←` / `→` | Previous / next image |
| `Esc` | Back to the gallery |

A mouse wheel and a trackpad swipe are told apart by their scroll unit:
wheels report discrete lines, trackpads report continuous points. A touch
mouse that reports points, such as a Magic Mouse, therefore pans.

Zoom stops once the whole image is visible; an image smaller than the
window stops at 1:1 rather than being upscaled to fill it.

The cache lives in the platform cache directory (on macOS,
`~/Library/Caches/dev.npv.nasa-photo-viewer`). Deleting it is safe.

## The API

That page is a front-end over a public, key-less, undocumented JSON endpoint:

```
GET https://mars.nasa.gov/rss/api/?feed=raw_images&category=mars2020&feedtype=json&num=100&page=0&order=sol+desc
```

It responds with `Content-Type: application/json`,
`Access-Control-Allow-Origin: *` and `Cache-Control: max-age=60`.

### Response shape

```json
{ "images": [...], "per_page": "3", "page": 0,
  "total_results": 563603, "total_images": 1029613,
  "type": "mars2020-images-list-1.1", "mission": "mars2020" }
```

Each record carries `imageid`, `sol`, `site`, `drive`, `date_taken_utc`,
`date_taken_mars`, `date_received`, `caption`, `credit`, `title`, plus nested
`image_files` (`small` 320px, `medium` 800px, `large` 1200px, `full_res` PNG),
`camera` (instrument, filter, CAHVORE model) and `extended` (dimensions,
subframe, spacecraft clock, mast azimuth/elevation).

### Parameters

| Parameter | Notes |
| --- | --- |
| `category` | `mars2020`. See the MSL note below. |
| `num` | Page size, hard-capped at 100. |
| `page` | 0-based. |
| `order` | `sol desc`, `sol asc`, `date_taken desc`. |
| `search` | Camera instrument names, OR-ed with `\|`. |
| `condition_1` | Mission scope: `mars2020:mission`. |
| `condition_2`, `condition_3`, … | Range filters: `100:sol:gte`, `2026-08-01:date_taken:lte`. |
| `id` | Single-image lookup. |

Camera names accepted by `search`: `NAVCAM_LEFT`, `NAVCAM_RIGHT`,
`FRONT_HAZCAM_LEFT_A`, `FRONT_HAZCAM_RIGHT_A`, `REAR_HAZCAM_LEFT`,
`REAR_HAZCAM_RIGHT`, `MCZ_LEFT`, `MCZ_RIGHT`, `SHERLOC_WATSON`,
`SUPERCAM_RMI`, `PIXL_MCC`, `SKYCAM`, `EDL_RUCAM`, `EDL_RDCAM`, `EDL_DDCAM`,
`EDL_PUCAM1`, `EDL_PUCAM2`, `LCAM`.

### Sharp edges

These are the behaviours this client exists to absorb:

- **`condition_1` is not a range slot.** A range placed there is silently
  ignored and the response comes back unfiltered with HTTP 200. Range filters
  must start at `condition_2`.
- **Multiple cameras join with `|`, not `,`.** A comma-separated list is
  accepted but matches zero images.
- **`num` is capped at 100.** Larger values are accepted and silently reduced,
  which breaks naive pagination arithmetic.
- **Running past the end still returns HTTP 200**, with an empty `images`
  array and `"error_message": "No more images."`.
- **Single-image lookups return `{"image": [...]}`**, not `{"images": [...]}`.
- **`total_results` counts records, `total_images` counts files.** They differ
  (563,603 vs 1,029,613) and are not interchangeable.
- **Types are inconsistent.** `per_page` and `drive` arrive as JSON strings
  even though they hold numbers, and older sols omit fields entirely.
- **Curiosity is not on this feed.** `category=msl` answers HTTP 200 with zero
  results forever; MSL raw images live at `/api/v1/raw_image_items/` with an
  incompatible schema.
- **Undocumented and unversioned.** `type` is the only version signal, and NASA
  can change any of the above without notice.

## Library

The feed client is usable on its own:

```rust
use nasa_photo_viewer::{Client, ImageSize, Query};

let client = Client::new()?;
let query = Query { min_sol: Some(1000), max_sol: Some(1000),
                    cameras: vec!["NAVCAM_LEFT".into()], ..Query::default() };

for image in client.list_all(&query, 50).await? {
    println!("{} {:?}", image.id(), image.url_for(ImageSize::Large));
}
```

## Tests

```bash
cargo test
```

Tests never touch the network. Parsing is checked against a recorded
response in `tests/fixtures/list_page.json`; the offline fallback is exercised
by pointing the client at a refused port; and the UI (thumbnail clicks,
keyboard navigation) is driven through `egui_kittest`.

## Courtesy

There is no published rate limit. The client sends a descriptive user agent and
you should keep request volume modest.
