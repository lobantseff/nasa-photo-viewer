//! Desktop viewer for NASA Mars 2020 (Perseverance) raw images.

use anyhow::{Context, Result};
use nasa_photo_viewer::app::App;
use nasa_photo_viewer::cache::Cache;

fn main() -> Result<()> {
    let cache = Cache::open_default().context("opening the local cache")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("NASA Photo Viewer — Perseverance"),
        ..Default::default()
    };

    eframe::run_native(
        "nasa-photo-viewer",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            match App::new(cc, cache) {
                Ok(app) => Ok(Box::new(app) as Box<dyn eframe::App>),
                Err(err) => Err(Box::<dyn std::error::Error + Send + Sync>::from(
                    err.to_string(),
                )),
            }
        }),
    )
    .map_err(|e| anyhow::anyhow!("running the viewer: {e}"))
}
