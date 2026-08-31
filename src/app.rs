//! The eframe application: filter sidebar, thumbnail gallery, detail viewer.

use std::collections::{HashMap, HashSet};

use egui::{Color32, Key, RichText, TextureHandle, TextureOptions, Vec2};

use crate::cache::Cache;
use crate::fetch::{Fetcher, ImageKind, Update};
use crate::model::{Image, ImageSize};
use crate::query::{MARS2020_CAMERAS, MAX_PAGE_SIZE, Order, Query};
use crate::viewer::{Gesture, ZoomPan, cursor_for, gesture_from, should_upgrade_to_full_res};

const THUMB_SIZE: f32 = 150.0;

/// Rows of thumbnails to load beyond the visible range. Fetching ahead of the
/// scroll position is what keeps the grid from stalling while scrolling.
const PREFETCH_ROWS: usize = 3;

/// Request the next page once this many images remain below the viewport.
const PAGE_LOOKAHEAD: usize = 30;

const STORAGE_KEY: &str = "npv_filters";

/// Back-navigation label.
///
/// egui's default fonts have no arrow glyphs (U+2190 and the emoji arrows all
/// render as tofu), so this uses a guillemet, which they do provide.
/// Lines a page-scroll stands for, on the rare device that reports pages.
const PAGE_LINES: f32 = 10.0;

const BACK_LABEL: &str = "\u{2039} Gallery";

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Filters {
    pub sol: String,
    pub cameras: Vec<String>,
    pub order: Order,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            sol: String::new(),
            cameras: Vec::new(),
            order: Order::SolDesc,
        }
    }
}

impl Filters {
    pub fn to_query(&self) -> Query {
        let sol = self.sol.trim().parse::<i64>().ok();
        Query {
            num: MAX_PAGE_SIZE,
            page: 0,
            order: self.order,
            cameras: self.cameras.clone(),
            min_sol: sol,
            max_sol: sol,
            taken_after: None,
            taken_before: None,
        }
    }
}

pub struct App {
    fetcher: Fetcher,
    filters: Filters,
    /// Cache key of the filters that produced `images`, so late responses for
    /// a filter set the user has moved on from are discarded.
    active_key: String,
    images: Vec<Image>,
    seen: HashSet<String>,
    total_results: Option<u64>,
    next_page: u64,
    exhausted: bool,
    textures: HashMap<String, TextureHandle>,
    selected: Option<usize>,
    zoom: ZoomPan,
    /// Size of the texture drawn last frame, so a resolution swap can keep the
    /// picture the same size on screen.
    shown_size: Option<Vec2>,
    full_res_pending: bool,
    error: Option<String>,
    serving_stale: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cache: Cache) -> anyhow::Result<Self> {
        let filters = cc
            .storage
            .and_then(|s| eframe::get_value::<Filters>(s, STORAGE_KEY))
            .unwrap_or_default();

        let mut app = Self::build(cc.egui_ctx.clone(), cache, filters)?;
        app.prime_from_cache();
        app.request_more();
        Ok(app)
    }

    /// Construct without touching storage or the network.
    fn build(ctx: egui::Context, cache: Cache, filters: Filters) -> anyhow::Result<Self> {
        let fetcher = Fetcher::new(ctx, cache)?;

        Ok(Self {
            active_key: filters.to_query().cache_key(),
            fetcher,
            filters,
            images: Vec::new(),
            seen: HashSet::new(),
            total_results: None,
            next_page: 0,
            exhausted: false,
            textures: HashMap::new(),
            selected: None,
            zoom: ZoomPan::default(),
            shown_size: None,
            full_res_pending: false,
            error: None,
            serving_stale: false,
        })
    }

    /// Paint instantly from cache when the first page is already stored.
    fn prime_from_cache(&mut self) {
        let query = self.filters.to_query();
        if let Some((images, total)) = self.fetcher.cached_listing(&query, 0) {
            self.absorb(images);
            self.total_results = total;
            self.next_page = 1;
        }
    }

    fn absorb(&mut self, images: Vec<Image>) {
        for image in images {
            if self.seen.insert(image.id().to_string()) {
                self.images.push(image);
            }
        }
    }

    fn reset_for_new_filters(&mut self) {
        self.active_key = self.filters.to_query().cache_key();
        self.images.clear();
        self.seen.clear();
        self.textures.clear();
        self.selected = None;
        self.total_results = None;
        self.next_page = 0;
        self.exhausted = false;
        self.serving_stale = false;
        self.error = None;
        self.prime_from_cache();
        self.request_more();
    }

    fn request_more(&mut self) {
        if self.exhausted {
            return;
        }
        let query = self.filters.to_query();
        let page = self.next_page;
        self.fetcher.request_listing(&query, page);
    }

    fn apply_updates(&mut self, ctx: &egui::Context) {
        for update in self.fetcher.poll() {
            match update {
                Update::Listing {
                    query_key,
                    page,
                    images,
                    total_results,
                    from_stale_cache,
                } => {
                    if query_key != self.active_key {
                        continue;
                    }
                    self.serving_stale = from_stale_cache;
                    self.total_results = total_results.or(self.total_results);

                    if images.is_empty() {
                        self.exhausted = true;
                    } else {
                        if page >= self.next_page {
                            self.next_page = page + 1;
                        }
                        self.absorb(images);
                    }
                }
                Update::Image { url, image, kind } => {
                    if kind == ImageKind::Full {
                        self.full_res_pending = false;
                    }
                    let handle = ctx.load_texture(url.clone(), *image, TextureOptions::LINEAR);
                    self.textures.insert(url, handle);
                }
                Update::Failed { error, .. } => self.error = Some(error),
                Update::Connectivity { .. } => {}
            }
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("filters")
            .resizable(false)
            .exact_size(230.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("Perseverance");
                ui.separator();

                let before = self.filters.clone();

                ui.label("Sol");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filters.sol)
                            .hint_text("latest")
                            .desired_width(110.0),
                    );
                    if ui.button("Clear").clicked() {
                        self.filters.sol.clear();
                    }
                });
                if !self.filters.sol.trim().is_empty()
                    && self.filters.sol.trim().parse::<i64>().is_err()
                {
                    ui.colored_label(Color32::LIGHT_RED, "Sol must be a number");
                }

                ui.add_space(10.0);
                ui.label("Order");
                egui::ComboBox::from_id_salt("order")
                    .selected_text(match self.filters.order {
                        Order::SolDesc => "Newest first",
                        Order::SolAsc => "Oldest first",
                        Order::DateTakenDesc => "Date taken",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.filters.order,
                            Order::SolDesc,
                            "Newest first",
                        );
                        ui.selectable_value(&mut self.filters.order, Order::SolAsc, "Oldest first");
                        ui.selectable_value(
                            &mut self.filters.order,
                            Order::DateTakenDesc,
                            "Date taken",
                        );
                    });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label("Cameras");
                    if !self.filters.cameras.is_empty() && ui.small_button("reset").clicked() {
                        self.filters.cameras.clear();
                    }
                });

                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for cam in MARS2020_CAMERAS {
                            let mut on = self.filters.cameras.iter().any(|c| c == cam);
                            if ui.checkbox(&mut on, *cam).changed() {
                                if on {
                                    self.filters.cameras.push((*cam).to_string());
                                } else {
                                    self.filters.cameras.retain(|c| c != cam);
                                }
                            }
                        }
                    });

                if self.filters != before {
                    self.reset_for_new_filters();
                }

                ui.add_space(12.0);
                ui.separator();
                match self.total_results {
                    Some(total) => ui.label(format!("{total} matching images")),
                    None => ui.label("Loading…"),
                };
                ui.label(format!("{} loaded", self.images.len()));
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.fetcher.is_online() {
                    status_dot(ui, Color32::from_rgb(90, 180, 110));
                    ui.colored_label(Color32::from_rgb(90, 180, 110), "online");
                } else {
                    status_dot(ui, Color32::from_rgb(230, 160, 60));
                    ui.colored_label(Color32::from_rgb(230, 160, 60), "offline");
                }

                if self.serving_stale {
                    ui.label("· showing cached results");
                }
                if self.fetcher.inflight_count() > 0 {
                    ui.spinner();
                    ui.label(format!("{} loading", self.fetcher.inflight_count()));
                }

                if let Some(err) = self.error.clone() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("dismiss").clicked() {
                            self.error = None;
                        }
                        ui.colored_label(Color32::LIGHT_RED, truncate(&err, 90));
                    });
                }
            });
        });
    }

    fn gallery(&mut self, ui: &mut egui::Ui) {
        if self.images.is_empty() {
            ui.centered_and_justified(|ui| {
                if self.fetcher.inflight_count() > 0 {
                    ui.label("Loading images…");
                } else {
                    ui.label("No images match these filters.");
                }
            });
            return;
        }

        let spacing = ui.spacing().item_spacing.x;
        let scroll = ui.spacing().scroll;
        let reserved =
            scrollbar_allowance(scroll.floating, scroll.bar_width, scroll.bar_inner_margin);
        let columns = columns_for(ui.available_width() - reserved, THUMB_SIZE, spacing);
        let cell = THUMB_SIZE + ui.spacing().item_spacing.y;
        let rows = self.images.len().div_ceil(columns);

        let mut clicked = None;
        let mut wanted: Vec<String> = Vec::new();

        // show_rows only builds the visible rows, so a set of thousands of
        // images costs the same to draw as a screenful.
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show_rows(ui, cell, rows, |ui, row_range| {
                let prefetch_end = (row_range.end + PREFETCH_ROWS).min(rows) * columns;
                let visible_end = (row_range.end * columns).min(self.images.len());

                for row in row_range.clone() {
                    ui.horizontal(|ui| {
                        for col in 0..columns {
                            let Some(idx) = index_at(row, col, columns, self.images.len()) else {
                                break;
                            };
                            if self.thumb(ui, idx, &mut wanted) {
                                clicked = Some(idx);
                            }
                        }
                    });
                }

                // Queue thumbnails slightly beyond the viewport.
                for idx in visible_end..prefetch_end.min(self.images.len()) {
                    if let Some(url) = self.images[idx].url_for(ImageSize::Small)
                        && !self.textures.contains_key(url)
                    {
                        wanted.push(url.to_string());
                    }
                }

                if visible_end + PAGE_LOOKAHEAD >= self.images.len() {
                    self.request_more();
                }
            });

        for url in wanted {
            self.fetcher.request_image(&url, ImageKind::Thumbnail);
        }
        if let Some(idx) = clicked {
            self.select(idx);
        }
    }

    /// Draw one thumbnail; returns true when clicked.
    fn thumb(&self, ui: &mut egui::Ui, idx: usize, wanted: &mut Vec<String>) -> bool {
        let image = &self.images[idx];
        let size = Vec2::splat(THUMB_SIZE);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        let clicked = response.clicked();

        if !ui.is_rect_visible(rect) {
            return clicked;
        }

        let url = image.url_for(ImageSize::Small);
        match url.and_then(|u| self.textures.get(u)) {
            Some(texture) => {
                let fitted = fit(texture.size_vec2(), size);
                let draw = egui::Rect::from_center_size(rect.center(), fitted);
                egui::Image::new(texture).paint_at(ui, draw);
            }
            None => {
                ui.painter()
                    .rect_filled(rect, 3.0, ui.visuals().faint_bg_color);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "…",
                    egui::FontId::proportional(16.0),
                    ui.visuals().weak_text_color(),
                );
                if let Some(url) = url {
                    wanted.push(url.to_string());
                }
            }
        }

        if response.hovered() {
            ui.painter().rect_stroke(
                rect,
                3.0,
                ui.visuals().widgets.hovered.fg_stroke,
                egui::StrokeKind::Inside,
            );
        }

        // Expose the image id to accessibility tooling, which also lets UI
        // tests find a specific thumbnail rather than guessing coordinates.
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), image.id())
        });

        response.on_hover_text(format!(
            "Sol {} · {}\n{}",
            image
                .sol
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into()),
            image.camera.instrument_or_unknown(),
            image.id()
        ));
        clicked
    }

    fn select(&mut self, idx: usize) {
        self.selected = Some(idx);
        self.zoom.reset();
        self.shown_size = None;
        self.full_res_pending = false;
        if let Some(url) = self.images[idx].url_for(ImageSize::Large)
            && !self.textures.contains_key(url)
        {
            let url = url.to_string();
            self.fetcher.request_image(&url, ImageKind::Thumbnail);
        }
    }

    fn step_selection(&mut self, delta: isize) {
        let Some(current) = self.selected else { return };
        let next = current as isize + delta;
        if next >= 0 && (next as usize) < self.images.len() {
            self.select(next as usize);
        }
    }

    fn detail(&mut self, ui: &mut egui::Ui, idx: usize) {
        let image = self.images[idx].clone();

        ui.horizontal(|ui| {
            if ui.button(BACK_LABEL).clicked() {
                self.selected = None;
            }
            ui.separator();
            if ui.button("Fit").clicked() {
                self.zoom.reset();
            }
            if ui.button("−").clicked() {
                let c = ui.max_rect().center();
                self.zoom.zoom_at(1.0 / 1.25, c, ui.max_rect());
            }
            if ui.button("+").clicked() {
                let c = ui.max_rect().center();
                self.zoom.zoom_at(1.25, c, ui.max_rect());
            }
            ui.label(format!("{:.0}%", self.zoom.scale * 100.0));

            ui.separator();
            let full_url = image.url_for(ImageSize::FullRes).map(|s| s.to_string());
            if let Some(full) = full_url.clone() {
                // Full resolution now arrives on its own once zoomed in; this
                // only reports where that has got to.
                if self.textures.contains_key(&full) {
                    ui.label(RichText::new("full resolution").italics());
                } else if self.full_res_pending {
                    ui.spinner();
                    ui.label(RichText::new("loading full resolution").italics());
                }

                if ui.button("Save…").clicked() {
                    let name = format!("{}.png", image.id());
                    if let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file()
                        && let Err(err) = self.fetcher.save_image_to(&full, path)
                    {
                        self.error = Some(err.to_string());
                    }
                }
            }
        });

        ui.separator();

        // Prefer the full-res texture once it has arrived.
        let large = image.url_for(ImageSize::Large).map(|s| s.to_string());
        let full = image.url_for(ImageSize::FullRes).map(|s| s.to_string());
        let texture = full
            .as_ref()
            .and_then(|u| self.textures.get(u))
            .or_else(|| large.as_ref().and_then(|u| self.textures.get(u)));

        let viewport = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(viewport, egui::Sense::click_and_drag());
        // A zoomed image is deliberately larger than the viewport, so all
        // painting must be clipped or it spills over the surrounding panels.
        let painter = ui.painter_at(viewport);

        let Some(texture) = texture else {
            painter.text(
                viewport.center(),
                egui::Align2::CENTER_CENTER,
                "Loading image…",
                egui::FontId::proportional(16.0),
                ui.visuals().weak_text_color(),
            );
            return;
        };

        let img_size = texture.size_vec2();
        if let Some(previous) = self.shown_size
            && previous != img_size
            && !self.zoom.needs_fit
        {
            self.zoom.preserve_apparent_size(previous, img_size);
        }
        self.shown_size = Some(img_size);

        if self.zoom.needs_fit {
            self.zoom.fit(img_size, viewport.size());
        } else {
            // The floor moves when the window resizes or a higher-resolution
            // rendition replaces the preview.
            self.zoom.set_bounds(img_size, viewport.size());
        }

        // Once the preview is magnified past its own pixels it looks soft, so
        // pull the original in automatically rather than making the user ask.
        if let Some(full) = full.as_ref()
            && should_upgrade_to_full_res(self.zoom.scale, self.zoom.min_scale())
            && !self.textures.contains_key(full)
        {
            self.full_res_pending = true;
            self.fetcher.request_image(full, ImageKind::Full);
        }

        let pannable = self.zoom.is_pannable(img_size, viewport.size());

        if response.dragged() {
            self.zoom.pan(response.drag_delta());
        }

        if response.hovered() {
            let (zoom_delta, wheel_lines, point_scroll, pointer) = ui.input(|i| {
                // Inspect raw events: the aggregated scroll delta has already
                // discarded the unit that tells a wheel from a trackpad.
                let mut lines = 0.0;
                let mut points = Vec2::ZERO;
                for event in &i.events {
                    if let egui::Event::MouseWheel { unit, delta, .. } = event {
                        match unit {
                            egui::MouseWheelUnit::Line => lines += delta.y,
                            egui::MouseWheelUnit::Page => lines += delta.y * PAGE_LINES,
                            egui::MouseWheelUnit::Point => points += *delta,
                        }
                    }
                }
                (i.zoom_delta(), lines, points, i.pointer.hover_pos())
            });

            match gesture_from(zoom_delta, wheel_lines, point_scroll) {
                Gesture::Zoom(factor) => {
                    let anchor = pointer.unwrap_or(viewport.center());
                    self.zoom.zoom_at(factor, anchor, viewport);
                }
                Gesture::Pan(delta) => self.zoom.pan(delta),
                Gesture::None => {}
            }
        }

        if (response.hovered() || response.dragged())
            && let Some(icon) = cursor_for(pannable, response.dragged())
        {
            ui.ctx().set_cursor_icon(icon);
        }

        self.zoom.clamp_to(img_size, viewport);

        let rect = self.zoom.image_rect(img_size, viewport);
        painter.rect_filled(viewport, 0.0, Color32::from_gray(18));
        painter.image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );

        // Metadata overlay.
        let text = format!(
            "Sol {} · {}\n{}\n{}",
            image
                .sol
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into()),
            image.camera.instrument_or_unknown(),
            image.date_taken_utc.as_deref().unwrap_or("-"),
            image.id(),
        );
        let pos = viewport.left_bottom() + Vec2::new(10.0, -10.0);
        painter.text(
            pos,
            egui::Align2::LEFT_BOTTOM,
            text,
            egui::FontId::monospace(11.0),
            Color32::from_gray(200),
        );
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, &self.filters);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_impl(ui);
    }
}

impl App {
    /// The whole UI, independent of eframe so it can be driven by tests.
    pub fn ui_impl(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.apply_updates(&ctx);

        ctx.input(|i| {
            if i.key_pressed(Key::Escape) {
                self.selected = None;
            }
        });
        if self.selected.is_some() {
            let (prev, next) = ctx.input(|i| {
                (
                    i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowRight),
                )
            });
            if prev {
                self.step_selection(-1);
            }
            if next {
                self.step_selection(1);
            }
        }

        self.sidebar(ui);
        self.status_bar(ui);

        egui::CentralPanel::default().show(ui, |ui| match self.selected {
            Some(idx) if idx < self.images.len() => self.detail(ui, idx),
            _ => self.gallery(ui),
        });
    }
}

/// Draw the connectivity dot.
///
/// Painted rather than written, because the obvious glyphs for it (U+25CF and
/// friends) are missing from egui's default fonts.
fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let diameter = 8.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(diameter), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), diameter * 0.5, color);
}

/// Layout width to keep clear for the scroll bar.
///
/// A floating bar is drawn over the content and takes no width of its own, so
/// reserving space for one loses a whole column on a wide window.
fn scrollbar_allowance(floating: bool, bar_width: f32, inner_margin: f32) -> f32 {
    if floating {
        0.0
    } else {
        bar_width + inner_margin
    }
}

/// How many thumbnails fit across `width`.
///
/// `n` thumbnails occupy `n * thumb + (n - 1) * spacing`, so the spacing has to
/// be added back before dividing or the last column is dropped.
fn columns_for(width: f32, thumb: f32, spacing: f32) -> usize {
    if !width.is_finite() || width <= 0.0 || thumb <= 0.0 {
        return 1;
    }
    (((width + spacing) / (thumb + spacing)).floor() as usize).max(1)
}

/// Index into the flat image list for a grid cell, if it exists.
fn index_at(row: usize, col: usize, columns: usize, len: usize) -> Option<usize> {
    let idx = row * columns + col;
    (idx < len).then_some(idx)
}

/// Scale `size` to fit inside `bounds` without distortion.
fn fit(size: Vec2, bounds: Vec2) -> Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return bounds;
    }
    let k = (bounds.x / size.x).min(bounds.y / size.y);
    size * k
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cache::{Cache, DEFAULT_CACHE_BUDGET};
    use crate::viewer::MAX_SCALE;
    use egui_kittest::kittest::Queryable as _;

    fn full_res_url(id: &str) -> String {
        format!("https://x/{id}_full.png")
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 200, 200, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// Build an app with pre-seeded images and textures, so rendering it
    /// performs no network access.
    fn test_app(ids: &[&str]) -> (App, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "npv-ui-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        // Pre-cache the full-resolution bytes so the automatic upgrade
        // resolves locally and the tests stay offline.
        for id in ids {
            cache
                .put_blob(&full_res_url(id), &png_bytes(128, 128))
                .unwrap();
        }

        let ctx = egui::Context::default();
        let mut app = App::build(ctx.clone(), cache, Filters::default()).unwrap();

        let images: Vec<Image> = ids
            .iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({
                    "imageid": id,
                    "sol": 1000,
                    "camera": { "instrument": "NAVCAM_LEFT" },
                    "image_files": {
                        "small": format!("https://x/{id}_320.jpg"),
                        "large": format!("https://x/{id}_1200.jpg"),
                        "full_res": full_res_url(id),
                    },
                }))
                .unwrap()
            })
            .collect();

        // Seed textures for every rendition the UI may request so no
        // background fetch is triggered during the test.
        for id in ids {
            for url in [
                format!("https://x/{id}_320.jpg"),
                format!("https://x/{id}_1200.jpg"),
            ] {
                // Large enough that zooming in genuinely overflows the
                // viewport, which is what makes panning possible.
                const N: usize = 256;
                let pixels = vec![200u8; N * N * 4];
                let tex = ctx.load_texture(
                    url.clone(),
                    egui::ColorImage::from_rgba_unmultiplied([N, N], &pixels),
                    egui::TextureOptions::LINEAR,
                );
                app.textures.insert(url, tex);
            }
        }

        app.absorb(images);
        // Stop the gallery from paging while under test.
        app.exhausted = true;
        (app, dir)
    }

    #[test]
    fn clicking_a_thumbnail_opens_the_detail_view() {
        let (app, dir) = test_app(&["ALPHA", "BETA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        assert_eq!(
            harness.state().selected,
            None,
            "should start on the gallery"
        );

        harness.get_by_label("BETA").click();
        settle(&mut harness);

        // Regression: the thumbnail used to swallow its own click, so the
        // detail view could never be opened.
        assert_eq!(harness.state().selected, Some(1));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_zoomed_image_never_paints_over_the_toolbar() {
        let (app, dir) = test_app(&["ALPHA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        // Zoom well past "fit" so the image is much larger than its viewport.
        harness.state_mut().zoom.needs_fit = false;
        harness.state_mut().zoom.scale = MAX_SCALE;
        settle(&mut harness);

        // Guard against a vacuous test: if the zoomed image fitted inside the
        // window there would be nothing for clipping to prevent.
        let shown = harness
            .state()
            .shown_size
            .expect("an image should be shown");
        let scaled = shown * harness.state().zoom.scale;
        let screen = harness.ctx.input(|i| i.viewport_rect());
        assert!(
            scaled.x > screen.width() && scaled.y > screen.height(),
            "zoomed image {scaled:?} fits inside {screen:?}, so clipping is untested"
        );

        let toolbar = harness.get_by_label("Fit").rect();

        // The image is intentionally larger than the viewport when zoomed, so
        // correctness depends entirely on it being clipped.
        let offending: Vec<_> = harness
            .output()
            .shapes
            .iter()
            .filter(|cs| matches!(cs.shape, egui::Shape::Mesh(_)))
            .filter(|cs| cs.clip_rect.intersects(toolbar))
            .collect();

        assert!(
            offending.is_empty(),
            "image mesh was allowed to paint over the toolbar at {toolbar:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Advance a bounded number of frames.
    ///
    /// `Harness::run` waits for the UI to go idle, which never happens while a
    /// loading spinner is animating.
    fn settle(harness: &mut egui_kittest::Harness<'_, App>) {
        harness.run_steps(8);
    }

    /// Hover over the middle of the image area, below the toolbar.
    fn hover_image_area(harness: &mut egui_kittest::Harness<'_, App>) -> egui::Pos2 {
        let toolbar = harness.get_by_label("Fit").rect();
        let pos = egui::pos2(toolbar.center().x + 200.0, toolbar.max.y + 250.0);
        harness.hover_at(pos);
        settle(harness);
        pos
    }

    #[test]
    fn a_pinch_gesture_zooms_the_image() {
        let (app, dir) = test_app(&["ALPHA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        hover_image_area(&mut harness);
        let before = harness.state().zoom.scale;

        // egui reports a trackpad pinch as a Zoom event.
        harness.event(egui::Event::Zoom(2.0));
        settle(&mut harness);

        assert!(
            harness.state().zoom.scale > before,
            "pinch should magnify: {before} -> {}",
            harness.state().zoom.scale
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zooming_in_requests_the_full_resolution_image_automatically() {
        // Only the preview renditions have textures; the original is cached
        // as bytes but not yet decoded.
        let (app, dir) = test_app(&["ALPHA"]);
        let full = full_res_url("ALPHA");
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        // Opening alone must not pull a multi-megabyte original.
        assert!(
            !harness.state().textures.contains_key(&full),
            "opening an image should not fetch full resolution"
        );

        hover_image_area(&mut harness);
        harness.state_mut().zoom.needs_fit = false;
        harness.state_mut().zoom.scale = 2.0;

        // The upgrade resolves from the cache on a worker thread, so give the
        // UI a bounded number of frames to receive it.
        let mut upgraded = false;
        for _ in 0..200 {
            settle(&mut harness);
            if harness.state().textures.contains_key(&full) {
                upgraded = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(
            upgraded,
            "magnifying the preview should load the full-resolution image"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_load_full_resolution_button_is_gone() {
        let (app, dir) = test_app(&["ALPHA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        assert!(
            harness.query_by_label("Load full resolution").is_none(),
            "the manual button should have been replaced by automatic loading"
        );
        // The save action must survive the removal.
        assert!(harness.query_by_label("Save\u{2026}").is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mouse_wheel_zooms_the_image() {
        let (app, dir) = test_app(&["ALPHA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        hover_image_area(&mut harness);
        let before = harness.state().zoom.scale;

        // A mouse wheel reports whole lines, unlike a trackpad's points.
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 3.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        settle(&mut harness);

        assert!(
            harness.state().zoom.scale > before,
            "the mouse wheel should zoom in: {before} -> {}",
            harness.state().zoom.scale
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_two_finger_scroll_pans_a_zoomed_image() {
        let (app, dir) = test_app(&["ALPHA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        hover_image_area(&mut harness);

        // Zoom in first, otherwise the image fits and panning is pinned.
        harness.state_mut().zoom.needs_fit = false;
        harness.state_mut().zoom.scale = MAX_SCALE;
        settle(&mut harness);
        let before = harness.state().zoom.offset;

        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -60.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        settle(&mut harness);

        assert_ne!(
            harness.state().zoom.offset,
            before,
            "a two-finger scroll should pan a zoomed image"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scrolling_does_not_move_an_image_that_already_fits() {
        let (app, dir) = test_app(&["ALPHA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        hover_image_area(&mut harness);

        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -60.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        settle(&mut harness);

        // The test image fits, so it stays pinned to the centre.
        assert_eq!(harness.state().zoom.offset, egui::Vec2::ZERO);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn escape_returns_from_the_detail_view_to_the_gallery() {
        let (app, dir) = test_app(&["ALPHA", "BETA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        harness.get_by_label("ALPHA").click();
        settle(&mut harness);
        assert_eq!(harness.state().selected, Some(0));

        harness.key_press(egui::Key::Escape);
        settle(&mut harness);
        assert_eq!(harness.state().selected, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn arrow_keys_step_through_images_in_the_detail_view() {
        let (app, dir) = test_app(&["ALPHA", "BETA", "GAMMA"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        harness.get_by_label("ALPHA").click();
        settle(&mut harness);

        harness.key_press(egui::Key::ArrowRight);
        settle(&mut harness);
        assert_eq!(harness.state().selected, Some(1));

        harness.key_press(egui::Key::ArrowLeft);
        settle(&mut harness);
        assert_eq!(harness.state().selected, Some(0));

        // Must not step past the start of the list.
        harness.key_press(egui::Key::ArrowLeft);
        settle(&mut harness);
        assert_eq!(harness.state().selected, Some(0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_ui_label_is_renderable_in_the_default_fonts() {
        // egui's default fonts lack many symbol glyphs, which silently render
        // as tofu boxes. Anything shown as text must be checked.
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |_| {});
        output.textures_delta.clear();

        let labels = [
            BACK_LABEL,
            "Fit",
            "\u{2212}",
            "+",
            "Load full resolution",
            "Save\u{2026}",
            "full resolution",
            "Gallery",
            "Clear",
            "reset",
            "dismiss",
            "online",
            "offline",
            "Perseverance",
            "Sol",
            "Order",
            "Cameras",
            "Newest first",
            "Oldest first",
            "Date taken",
            "Loading\u{2026}",
            "Loading image\u{2026}",
            "No images match these filters.",
            "\u{2026}",
            "Sol must be a number",
            "\u{00b7} showing cached results",
            "Sol 1000 \u{00b7} NAVCAM_LEFT",
        ];

        let font = egui::FontId::proportional(14.0);
        ctx.fonts_mut(|f| {
            for label in labels {
                for c in label.chars().filter(|c| !c.is_whitespace()) {
                    assert!(
                        f.has_glyph(&font, c),
                        "{c:?} (U+{:04X}) in {label:?} has no glyph and will render as tofu",
                        c as u32
                    );
                }
            }
        });
    }

    #[test]
    fn camera_names_are_renderable() {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |_| {});
        output.textures_delta.clear();

        let font = egui::FontId::proportional(14.0);
        ctx.fonts_mut(|f| {
            for cam in MARS2020_CAMERAS {
                for c in cam.chars() {
                    assert!(f.has_glyph(&font, c), "{c:?} in {cam} has no glyph");
                }
            }
        });
    }

    #[test]
    fn grid_indexing_stops_at_the_end_of_the_list() {
        assert_eq!(index_at(0, 0, 4, 10), Some(0));
        assert_eq!(index_at(1, 2, 4, 10), Some(6));
        assert_eq!(index_at(2, 3, 4, 10), None);
    }

    #[test]
    fn column_count_accounts_for_inter_item_spacing() {
        // Exactly four 150px thumbnails plus three 10px gaps = 630px.
        assert_eq!(columns_for(630.0, 150.0, 10.0), 4);
        // One pixel short still fits four, because the trailing gap is unused.
        assert_eq!(columns_for(629.0, 150.0, 10.0), 3);
        // A whole extra cell needs both the thumbnail and its leading gap.
        assert_eq!(columns_for(790.0, 150.0, 10.0), 5);
    }

    #[test]
    fn a_floating_scroll_bar_reserves_no_width() {
        // egui's default bar floats over the content.
        assert_eq!(scrollbar_allowance(true, 10.0, 4.0), 0.0);
        assert_eq!(scrollbar_allowance(false, 10.0, 4.0), 14.0);
    }

    #[test]
    fn a_wide_window_uses_every_column_that_fits() {
        // Eight 150px thumbnails and seven 8px gaps need 1256px. Reserving
        // width for a floating scroll bar used to drop this to seven,
        // leaving an empty column on the right.
        assert_eq!(columns_for(1262.0, 150.0, 8.0), 8);
        assert_eq!(columns_for(1262.0 - 14.0, 150.0, 8.0), 7);
    }

    #[test]
    fn column_count_never_drops_below_one() {
        for width in [-100.0, 0.0, 10.0, f32::NAN, f32::INFINITY] {
            assert!(columns_for(width, 150.0, 10.0) >= 1, "width {width}");
        }
    }

    #[test]
    fn fit_preserves_aspect_ratio() {
        let got = fit(Vec2::new(200.0, 100.0), Vec2::splat(50.0));
        assert_eq!(got, Vec2::new(50.0, 25.0));
    }

    #[test]
    fn fit_handles_a_degenerate_size() {
        assert_eq!(fit(Vec2::ZERO, Vec2::splat(10.0)), Vec2::splat(10.0));
    }

    #[test]
    fn filters_map_a_sol_to_both_bounds() {
        let f = Filters {
            sol: " 1000 ".into(),
            cameras: vec!["NAVCAM_LEFT".into()],
            order: Order::SolDesc,
        };
        let q = f.to_query();

        assert_eq!(q.min_sol, Some(1000));
        assert_eq!(q.max_sol, Some(1000));
        assert_eq!(q.cameras, vec!["NAVCAM_LEFT".to_string()]);
    }

    #[test]
    fn a_blank_or_invalid_sol_means_no_sol_filter() {
        for text in ["", "   ", "abc"] {
            let f = Filters {
                sol: text.into(),
                ..Filters::default()
            };
            assert_eq!(f.to_query().min_sol, None, "input {text:?}");
        }
    }

    #[test]
    fn changing_filters_changes_the_cache_key() {
        let a = Filters::default();
        let b = Filters {
            sol: "1000".into(),
            ..Filters::default()
        };
        assert_ne!(a.to_query().cache_key(), b.to_query().cache_key());
    }

    #[test]
    fn truncates_long_errors() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("ab", 5), "ab");
    }
}
