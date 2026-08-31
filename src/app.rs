//! The eframe application: filter sidebar, thumbnail gallery, detail viewer.

use std::collections::HashSet;

use egui::{Color32, Key, RichText, TextureOptions, Vec2};

use crate::cache::Cache;
use crate::fetch::{Fetcher, ImageKind, Update};
use crate::model::{Image, ImageSize};
use crate::query::{MARS2020_CAMERAS, MAX_PAGE_SIZE, Order, Query};
use crate::textures::{TextureStore, Tier};
use crate::viewer::{Gesture, ZoomPan, cursor_for, gesture_from, should_upgrade_to_full_res};

const THUMB_SIZE: f32 = 150.0;

/// Rows of thumbnails to load beyond the visible range. Fetching ahead of the
/// scroll position is what keeps the grid from stalling while scrolling.
const PREFETCH_ROWS: usize = 3;

/// Request the next page once this many images remain below the viewport.
///
/// Every page costs the same large fixed delay upstream, so the request has to
/// start roughly a full page before it is needed; a short runway means
/// scrolling always ends in a wait.
const PAGE_LOOKAHEAD: usize = MAX_PAGE_SIZE as usize + 20;

const STORAGE_KEY: &str = "npv_filters";

/// Persisted separately from the filters: with a sol filter restored at
/// launch, every image loaded is from that one day, so the slider's upper
/// bound cannot be recovered from the results.
const LATEST_SOL_KEY: &str = "npv_latest_sol";

/// Back-navigation label.
///
/// egui's default fonts have no arrow glyphs (U+2190 and the emoji arrows all
/// render as tofu), so this uses a guillemet, which they do provide.
/// Progress of the full-resolution original for the open image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullResStatus {
    Loading,
    Loaded,
}

/// What the detail view managed to draw this frame.
///
/// Painted text is invisible to the accessibility tree, so this is what lets
/// tests tell a stand-in apart from an empty "loading" panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailContent {
    /// Nothing decoded yet: the loading panel.
    #[default]
    Missing,
    /// The gallery thumbnail, held up until the real rendition lands.
    StandIn,
    /// The detail or full-resolution rendition.
    Rendition,
}

/// How many images either side of the selection to fetch ahead.
///
/// Must stay below the detail texture budget, or stepping forward would evict
/// the images just fetched behind.
const PREFETCH_RADIUS: usize = 3;

/// Lines a page-scroll stands for, on the rare device that reports pages.
const PAGE_LINES: f32 = 10.0;

const BACK_LABEL: &str = "\u{2039} Gallery";

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Filters {
    /// A single Martian day to show, or `None` for every sol.
    pub sol: Option<i64>,
    pub cameras: Vec<String>,
    pub order: Order,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            sol: None,
            cameras: Vec::new(),
            order: Order::SolDesc,
        }
    }
}

impl Filters {
    pub fn to_query(&self) -> Query {
        let sol = self.sol;
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
    textures: TextureStore,
    selected: Option<usize>,
    zoom: ZoomPan,
    /// Size of the texture drawn last frame, so a resolution swap can keep the
    /// picture the same size on screen.
    shown_size: Option<Vec2>,
    full_res_pending: bool,
    /// A right-arrow press that ran past the loaded batch, waiting on a page.
    pending_advance: bool,
    /// Highest sol observed, which bounds the sol slider.
    latest_sol: Option<i64>,
    detail_content: DetailContent,
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
        app.latest_sol = cc
            .storage
            .and_then(|s| eframe::get_value::<Option<i64>>(s, LATEST_SOL_KEY))
            .flatten();
        app.prime_from_cache();
        app.request_more();
        Ok(app)
    }

    /// Construct without touching storage or the network.
    fn build(ctx: egui::Context, cache: Cache, filters: Filters) -> anyhow::Result<Self> {
        Self::from_fetcher(Fetcher::new(ctx, cache)?, filters)
    }

    fn from_fetcher(fetcher: Fetcher, filters: Filters) -> anyhow::Result<Self> {
        Ok(Self {
            active_key: filters.to_query().cache_key(),
            fetcher,
            filters,
            images: Vec::new(),
            seen: HashSet::new(),
            total_results: None,
            next_page: 0,
            exhausted: false,
            textures: TextureStore::default(),
            selected: None,
            zoom: ZoomPan::default(),
            shown_size: None,
            full_res_pending: false,
            pending_advance: false,
            latest_sol: None,
            detail_content: DetailContent::default(),
            error: None,
            serving_stale: false,
        })
    }

    /// Paint instantly from cache, refreshing behind the UI if it is stale.
    ///
    /// Upstream is slow enough on a cold query that waiting for it leaves the
    /// window empty for many seconds, so anything already stored is shown
    /// first and corrected once the response lands.
    fn prime_from_cache(&mut self) {
        let query = self.filters.to_query();
        let Some(cached) = self.fetcher.cached_listing(&query, 0) else {
            return;
        };

        self.serving_stale = cached.stale;
        self.total_results = cached.total_results;
        self.absorb(cached.images);
        self.next_page = 1;

        if cached.stale {
            // Page 0 is what the user is looking at, so refresh that rather
            // than only paging onwards.
            self.fetcher.request_listing(&query, 0);
        }
    }

    fn absorb(&mut self, images: Vec<Image>) {
        for image in images {
            // Kept monotonic: filtering to one sol would otherwise shrink the
            // slider's range to that day.
            if let Some(sol) = image.sol {
                self.latest_sol = Some(self.latest_sol.map_or(sol, |seen| seen.max(sol)));
            }
            if self.seen.insert(image.id().to_string()) {
                self.images.push(image);
            }
        }
    }

    fn reset_for_new_filters(&mut self) {
        self.active_key = self.filters.to_query().cache_key();
        self.images.clear();
        self.seen.clear();
        // Textures are keyed by URL and bounded by their own budget, so
        // keeping them lets an overlapping filter redraw instantly.
        self.selected = None;
        self.total_results = None;
        self.next_page = 0;
        self.exhausted = false;
        self.serving_stale = false;
        self.pending_advance = false;
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
                    self.merge_listing(page, from_stale_cache, images, total_results);
                }
                Update::Image { url, image, kind } => {
                    if kind == ImageKind::Full {
                        self.full_res_pending = false;
                    }
                    let tier = match kind {
                        ImageKind::Thumbnail => Tier::Thumbnail,
                        ImageKind::Detail | ImageKind::Full => Tier::Detail,
                    };
                    let handle = ctx.load_texture(url.clone(), *image, TextureOptions::LINEAR);
                    self.textures.insert(url, handle, tier);
                }
                Update::Failed { error, .. } => self.error = Some(error),
                Update::Connectivity { .. } => {}
            }
        }
    }

    /// Where the full-resolution original has got to for the open image.
    ///
    /// Only images that genuinely publish one qualify: `url_for` falls back to
    /// smaller renditions, so asking it would claim full resolution for an
    /// image that has none.
    fn full_res_status(&self) -> Option<FullResStatus> {
        let image = self.images.get(self.selected?)?;
        let url = image.image_files.full_res.as_deref()?;

        if self.textures.contains(url) {
            Some(FullResStatus::Loaded)
        } else if self.full_res_pending {
            Some(FullResStatus::Loading)
        } else {
            None
        }
    }

    /// Fold a listing page into the displayed set.
    fn merge_listing(
        &mut self,
        page: u64,
        from_stale_cache: bool,
        images: Vec<Image>,
        total_results: Option<u64>,
    ) {
        // A freshly fetched first page supersedes whatever was shown from
        // cache; appending would interleave the two orderings instead of
        // replacing, stranding the stale items at the top.
        if page == 0 && !from_stale_cache && self.serving_stale {
            self.images.clear();
            self.seen.clear();
        }
        self.serving_stale = from_stale_cache;
        self.total_results = total_results.or(self.total_results);

        if images.is_empty() {
            self.exhausted = true;
            return;
        }
        if page >= self.next_page {
            self.next_page = page + 1;
        }
        self.absorb(images);
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

                ui.horizontal(|ui| {
                    ui.label("Sol");
                    ui.label(RichText::new("(Martian day; 0 is landing)").weak().small());
                });

                match self.latest_sol {
                    Some(latest) => {
                        // The slider's value box accepts typing, so the exact
                        // sol is still reachable without dragging.
                        let mut value = self.filters.sol.unwrap_or(latest).clamp(0, latest);
                        let changed = ui
                            .add(egui::Slider::new(&mut value, 0..=latest).step_by(1.0))
                            .changed();
                        if changed {
                            self.filters.sol = Some(value);
                        }

                        ui.horizontal(|ui| {
                            if self.filters.sol.is_some() {
                                if ui.button("All sols").clicked() {
                                    self.filters.sol = None;
                                }
                            } else {
                                ui.label(RichText::new("all sols").weak().small());
                            }
                        });
                    }
                    None => {
                        ui.label(
                            RichText::new("waiting for the first results")
                                .weak()
                                .small(),
                        );
                    }
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
                    let refreshing = self.fetcher.inflight_count() > 0;
                    ui.label(if refreshing {
                        "· cached results, refreshing"
                    } else {
                        "· cached results"
                    });
                }
                // Report the two kinds of work separately: a lone slow page
                // fetch and a burst of thumbnails look identical otherwise.
                let listings = self.fetcher.inflight_listings();
                let images = self.fetcher.inflight_images();
                if listings > 0 || images > 0 {
                    ui.spinner();
                }
                if listings > 0 {
                    ui.label("fetching more results…");
                }
                if images > 0 {
                    ui.label(format!("{images} image(s) loading"));
                }

                let full_res = self.full_res_status();
                let error = self.error.clone();
                if full_res.is_some() || error.is_some() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Added first, so it sits hard against the right edge
                        // and keeps a fixed home regardless of what else the
                        // status bar is saying.
                        match full_res {
                            Some(FullResStatus::Loaded) => {
                                ui.label(RichText::new("full resolution").italics());
                            }
                            Some(FullResStatus::Loading) => {
                                ui.label(RichText::new("loading full resolution").italics());
                                ui.spinner();
                            }
                            None => {}
                        }

                        if let Some(err) = error {
                            if ui.small_button("dismiss").clicked() {
                                self.error = None;
                            }
                            ui.colored_label(Color32::LIGHT_RED, truncate(&err, 90));
                        }
                    });
                }
            });
        });
    }

    fn gallery(&mut self, ui: &mut egui::Ui) {
        if self.images.is_empty() {
            ui.centered_and_justified(|ui| {
                if self.fetcher.inflight_count() > 0 {
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.label("Loading images…");
                        ui.label(
                            RichText::new(
                                "NASA's first response for a new filter can take a while",
                            )
                            .weak()
                            .small(),
                        );
                    });
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
                        && !self.textures.contains(url)
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
    fn thumb(&mut self, ui: &mut egui::Ui, idx: usize, wanted: &mut Vec<String>) -> bool {
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
                egui::Image::new(&texture).paint_at(ui, draw);
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
        self.pending_advance = false;
        self.zoom.reset();
        self.shown_size = None;
        self.full_res_pending = false;
        self.request_detail(idx);
        // Warm the images either side so arrow-key browsing does not wait on
        // the network at every step.
        for offset in 1..=PREFETCH_RADIUS {
            if let Some(before) = idx.checked_sub(offset) {
                self.request_detail(before);
            }
            self.request_detail(idx + offset);
        }
    }

    /// Request the detail rendition for `idx`, if not already decoded.
    fn request_detail(&mut self, idx: usize) {
        let Some(image) = self.images.get(idx) else {
            return;
        };
        let Some(url) = image.url_for(ImageSize::Large) else {
            return;
        };
        if self.textures.contains(url) {
            return;
        }
        let url = url.to_string();
        self.fetcher.request_image(&url, ImageKind::Detail);
    }

    fn step_selection(&mut self, delta: isize) {
        let Some(current) = self.selected else { return };
        let next = current as isize + delta;
        if next < 0 {
            return;
        }
        let next = next as usize;

        if next < self.images.len() {
            self.select(next);
        } else if !self.exhausted {
            // Stepping off the end is a clear request to keep going, so carry
            // the intent until the next page lands.
            self.pending_advance = true;
        }

        // Paging is otherwise driven by the gallery's scroll position, which
        // is not running while the detail view is open. Without this, browsing
        // by keyboard stops dead at the edge of the loaded batch.
        if next + PAGE_LOOKAHEAD >= self.images.len() {
            self.request_more();
        }
    }

    /// Resume a step that ran off the end once more images have arrived.
    fn resume_pending_advance(&mut self) {
        if !self.pending_advance {
            return;
        }
        let Some(current) = self.selected else {
            self.pending_advance = false;
            return;
        };
        if current + 1 < self.images.len() {
            self.pending_advance = false;
            self.select(current + 1);
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
                // Progress is reported in the status bar: a label appearing
                // here would shift the buttons beside it.
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

        let small = image.url_for(ImageSize::Small).map(|s| s.to_string());
        let large = image.url_for(ImageSize::Large).map(|s| s.to_string());
        let full = image.url_for(ImageSize::FullRes).map(|s| s.to_string());

        // Prefer the sharpest rendition already decoded. Falling back to the
        // gallery thumbnail means stepping between images shows something
        // immediately instead of an empty panel.
        let mut stand_in = false;
        let texture = full
            .as_ref()
            .and_then(|u| self.textures.get(u))
            .or_else(|| large.as_ref().and_then(|u| self.textures.get(u)))
            .or_else(|| {
                let thumb = small.as_ref().and_then(|u| self.textures.get(u));
                stand_in = thumb.is_some();
                thumb
            });

        let viewport = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(viewport, egui::Sense::click_and_drag());
        // A zoomed image is deliberately larger than the viewport, so all
        // painting must be clipped or it spills over the surrounding panels.
        let painter = ui.painter_at(viewport);

        self.detail_content = match (&texture, stand_in) {
            (Some(_), true) => DetailContent::StandIn,
            (Some(_), false) => DetailContent::Rendition,
            (None, _) => DetailContent::Missing,
        };

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

        // A stand-in must fill the space the real image will occupy, so it is
        // allowed to upscale past 1:1 as the real rendition would not be.
        if self.zoom.needs_fit {
            self.zoom.fit(img_size, viewport.size(), stand_in);
        } else {
            // The floor moves when the window resizes or a higher-resolution
            // rendition replaces the preview.
            self.zoom.set_bounds(img_size, viewport.size(), stand_in);
        }

        // Once the preview is magnified past its own pixels it looks soft, so
        // pull the original in automatically rather than making the user ask.
        if let Some(full) = full.as_ref()
            && should_upgrade_to_full_res(self.zoom.scale, self.zoom.min_scale())
            && !self.textures.contains(full)
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
        eframe::set_value(storage, LATEST_SOL_KEY, &self.latest_sol);
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
        if self.selected.is_none() {
            self.pending_advance = false;
        }
        self.resume_pending_advance();
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

    /// An app whose fetcher points at a refused port, so no test can reach
    /// the real service.
    fn offline_app(ctx: egui::Context, cache: Cache) -> App {
        let client = crate::client::Client::with_endpoint("http://127.0.0.1:1/").unwrap();
        let fetcher = Fetcher::with_client(ctx, cache, client).unwrap();
        App::from_fetcher(fetcher, Filters::default()).unwrap()
    }

    fn temp_cache_dir() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "npv-stale-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sol_image(id: &str, sol: i64) -> Image {
        serde_json::from_value(serde_json::json!({
            "imageid": id,
            "sol": sol,
            "camera": { "instrument": "NAVCAM_LEFT" },
        }))
        .unwrap()
    }

    fn test_image(id: &str) -> Image {
        serde_json::from_value(serde_json::json!({
            "imageid": id,
            "sol": 1000,
            "camera": { "instrument": "NAVCAM_LEFT" },
            "image_files": { "small": thumb_url(id), "large": large_url(id) },
        }))
        .unwrap()
    }

    fn thumb_url(id: &str) -> String {
        format!("https://x/{id}_320.jpg")
    }

    fn large_url(id: &str) -> String {
        format!("https://x/{id}_1200.jpg")
    }

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
        let mut app = offline_app(ctx.clone(), cache);

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
                let tier = if url.contains("_320") {
                    Tier::Thumbnail
                } else {
                    Tier::Detail
                };
                app.textures.insert(url, tex, tier);
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
            !harness.state().textures.contains(&full),
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
            if harness.state().textures.contains(&full) {
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

    /// An app whose thumbnails are decoded but whose detail renditions are
    /// not, which is the state after a plain gallery scroll.
    fn test_app_thumbs_only(ids: &[&str]) -> (App, std::path::PathBuf) {
        let (mut app, dir) = test_app(ids);
        app.textures = TextureStore::default();

        let ctx = egui::Context::default();
        for id in ids {
            let tex = ctx.load_texture(
                thumb_url(id),
                egui::ColorImage::from_rgba_unmultiplied([64, 64], &vec![180u8; 64 * 64 * 4]),
                egui::TextureOptions::LINEAR,
            );
            app.textures.insert(thumb_url(id), tex, Tier::Thumbnail);
        }
        (app, dir)
    }

    /// Age every cached listing past its refresh window.
    fn expire_cached_listings(dir: &std::path::Path) {
        let db = rusqlite::Connection::open(dir.join("metadata.sqlite")).unwrap();
        db.execute(
            "UPDATE listings SET fetched_at = fetched_at - ?1",
            [crate::cache::LISTING_TTL_SECS * 10],
        )
        .unwrap();
    }

    #[test]
    fn expired_cached_results_are_shown_instead_of_a_blank_screen() {
        // Populate the cache, then age it past its refresh window.
        let dir = temp_cache_dir();
        let query = Filters::default().to_query();
        let images: Vec<Image> = ["OLD1", "OLD2"].iter().map(|id| test_image(id)).collect();
        {
            let mut cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
            cache.put_listing(&query, 0, Some(2), &images).unwrap();
        }
        expire_cached_listings(&dir);

        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let ctx = egui::Context::default();
        let mut app = offline_app(ctx, cache);
        app.prime_from_cache();

        // Upstream can take many seconds; the user should not stare at an
        // empty window in the meantime.
        assert_eq!(app.images.len(), 2, "stale results should be shown at once");
        assert!(app.serving_stale, "and be flagged as cached");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_fresh_first_page_replaces_the_stale_one_rather_than_appending() {
        let dir = temp_cache_dir();
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let ctx = egui::Context::default();
        let mut app = offline_app(ctx, cache);

        app.serving_stale = true;
        app.absorb(vec![test_image("OLD1"), test_image("OLD2")]);

        app.merge_listing(
            0,
            false,
            vec![test_image("NEW1"), test_image("NEW2")],
            Some(2),
        );

        // Appending would interleave two orderings and leave the stale items
        // stranded at the top.
        let ids: Vec<String> = app.images.iter().map(|i| i.id().to_string()).collect();
        assert_eq!(ids, vec!["NEW1", "NEW2"]);
        assert!(!app.serving_stale);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_later_fresh_page_appends_rather_than_replacing() {
        let dir = temp_cache_dir();
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let ctx = egui::Context::default();
        let mut app = offline_app(ctx, cache);

        app.serving_stale = true;
        app.absorb(vec![test_image("A")]);
        app.merge_listing(1, false, vec![test_image("B")], Some(2));

        assert_eq!(app.images.len(), 2, "page 1 must extend, not replace");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changing_filters_keeps_decoded_thumbnails() {
        let (mut app, dir) = test_app_thumbs_only(&["A", "B"]);
        let before = app.textures.count(Tier::Thumbnail);
        assert!(before > 0);

        app.filters.sol = Some(1000);
        app.reset_for_new_filters();

        // Filters usually overlap, so discarding decoded thumbnails would
        // force them all to be fetched and decoded again.
        assert_eq!(app.textures.count(Tier::Thumbnail), before);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn full_resolution_progress_is_reported_only_for_the_open_image() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);

        // Nothing open.
        assert_eq!(app.full_res_status(), None);

        app.selected = Some(0);
        assert_eq!(app.full_res_status(), None, "idle, so nothing to report");

        app.full_res_pending = true;
        assert_eq!(app.full_res_status(), Some(FullResStatus::Loading));

        let ctx = egui::Context::default();
        let tex = ctx.load_texture(
            "f",
            egui::ColorImage::from_rgba_unmultiplied([1, 1], &[1, 2, 3, 255]),
            egui::TextureOptions::LINEAR,
        );
        app.textures.insert(full_res_url("A"), tex, Tier::Detail);
        assert_eq!(app.full_res_status(), Some(FullResStatus::Loaded));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_image_without_an_original_never_claims_full_resolution() {
        let dir = temp_cache_dir();
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let mut app = offline_app(egui::Context::default(), cache);

        // `url_for` falls back to smaller renditions, so asking it would
        // report full resolution for an image that publishes none.
        app.absorb(vec![test_image("A")]);
        app.selected = Some(0);
        app.full_res_pending = true;

        assert_eq!(app.full_res_status(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_detail_toolbar_keeps_a_fixed_set_of_controls() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.selected = Some(0);

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        // The progress text lives in the status bar now; in the toolbar it
        // would appear and vanish, shifting the buttons beside it.
        harness.state_mut().full_res_pending = true;
        settle(&mut harness);

        for label in ["Fit", "+", "Save…"] {
            assert!(
                harness.query_by_label(label).is_some(),
                "{label} should still be present"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn arrowing_off_the_end_asks_for_the_next_page() {
        let (mut app, dir) = test_app_thumbs_only(&["A", "B", "C"]);
        app.exhausted = false;
        app.next_page = 1;

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("C").click();
        settle(&mut harness);

        let before = harness.state().fetcher.issued_count();
        harness.key_press(egui::Key::ArrowRight);
        settle(&mut harness);

        // Paging is driven by the gallery's scroll position, which does not
        // run in the detail view; without this, keyboard browsing stops at
        // the edge of the loaded batch.
        assert!(
            harness.state().fetcher.issued_count() > before,
            "stepping past the last image should request more"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_step_past_the_end_resumes_when_the_page_arrives() {
        let (mut app, dir) = test_app_thumbs_only(&["A", "B"]);
        app.exhausted = false;
        app.selected = Some(1);
        app.pending_advance = true;

        app.merge_listing(1, false, vec![test_image("C")], Some(3));
        app.resume_pending_advance();

        assert_eq!(
            app.selected,
            Some(2),
            "browsing should carry on to the new image"
        );
        assert!(!app.pending_advance);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaving_the_viewer_cancels_a_queued_step() {
        let (mut app, dir) = test_app_thumbs_only(&["A", "B"]);
        app.exhausted = false;
        app.selected = Some(1);
        app.pending_advance = true;

        // Back to the gallery: a page arriving later must not yank the user
        // into an image they no longer had open.
        app.selected = None;
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        harness
            .state_mut()
            .merge_listing(1, false, vec![test_image("C")], Some(3));
        settle(&mut harness);

        assert_eq!(harness.state().selected, None);
        assert!(!harness.state().pending_advance);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stepping_backwards_from_the_first_image_stays_put() {
        let (mut app, dir) = test_app_thumbs_only(&["A", "B"]);
        app.selected = Some(0);

        app.step_selection(-1);

        assert_eq!(app.selected, Some(0));
        assert!(!app.pending_advance, "going back should never queue a step");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_next_page_is_requested_well_before_the_end_of_the_batch() {
        // A full page of results, viewed from the top.
        let ids: Vec<String> = (0..100).map(|i| format!("P{i:03}")).collect();
        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let (mut app, dir) = test_app_thumbs_only(&refs);
        app.exhausted = false;
        app.next_page = 1;

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        // Each page costs the same long delay upstream, so the fetch has to
        // begin about a page early rather than as the last row appears.
        assert!(
            harness.state().fetcher.issued_count() > 0,
            "the next page should already be on its way while the user is \
             still near the top of the current one"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn selecting_an_image_prefetches_its_neighbours() {
        // Eight images, selecting the fourth: the window should reach three
        // either side and stop, leaving the last untouched.
        let (app, dir) = test_app_thumbs_only(&["A", "B", "C", "D", "E", "F", "G", "H"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        // Requests drain as they fail, so count dispatches rather than
        // whatever happens to be in flight at the moment of the assertion.
        let before = harness.state().fetcher.issued_count();
        harness.get_by_label("D").click();
        settle(&mut harness);
        let issued = harness.state().fetcher.issued_count() - before;

        // A through G, and not H.
        let expected = 1 + 2 * PREFETCH_RADIUS as u64;
        assert_eq!(
            issued, expected,
            "should fetch the selection plus {PREFETCH_RADIUS} either side"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stepping_to_a_neighbour_shows_its_thumbnail_rather_than_a_loading_screen() {
        let (app, dir) = test_app_thumbs_only(&["A", "B", "C"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("B").click();
        settle(&mut harness);

        // The detail rendition is absent, so without the fallback this would
        // be the empty "Loading image…" panel.
        assert!(!harness.state().textures.contains(&large_url("B")));
        assert_eq!(
            harness.state().detail_content,
            DetailContent::StandIn,
            "a decoded thumbnail should stand in while the detail loads"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn browsing_many_images_does_not_grow_textures_without_bound() {
        let ids: Vec<String> = (0..40).map(|i| format!("IMG{i:02}")).collect();
        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let (app, dir) = test_app(&refs);

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        harness.get_by_label("IMG00").click();
        settle(&mut harness);

        for _ in 0..39 {
            harness.key_press(egui::Key::ArrowRight);
            settle(&mut harness);
        }

        let details = harness.state().textures.count(Tier::Detail);
        assert!(
            details <= crate::textures::DEFAULT_DETAIL_CAPACITY,
            "detail textures grew to {details}, past the budget"
        );

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
            "Save\u{2026}",
            "full resolution",
            "loading full resolution",
            "fetching more results…",
            "Gallery",
            "Clear",
            "reset",
            "dismiss",
            "online",
            "offline",
            "Perseverance",
            "Sol",
            "(Martian day; 0 is landing)",
            "All sols",
            "all sols",
            "waiting for the first results",
            "Order",
            "Cameras",
            "Newest first",
            "Oldest first",
            "Date taken",
            "Loading\u{2026}",
            "Loading image\u{2026}",
            "No images match these filters.",
            "\u{2026}",
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
            sol: Some(1000),
            cameras: vec!["NAVCAM_LEFT".into()],
            order: Order::SolDesc,
        };
        let q = f.to_query();

        // A single Martian day is a range of one.
        assert_eq!(q.min_sol, Some(1000));
        assert_eq!(q.max_sol, Some(1000));
        assert_eq!(q.cameras, vec!["NAVCAM_LEFT".to_string()]);
    }

    #[test]
    fn no_sol_selected_means_every_sol() {
        let q = Filters::default().to_query();

        assert_eq!(q.min_sol, None);
        assert_eq!(q.max_sol, None);
    }

    #[test]
    fn the_newest_sol_seen_never_decreases() {
        let dir = temp_cache_dir();
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let mut app = offline_app(egui::Context::default(), cache);

        app.absorb(vec![sol_image("A", 1900), sol_image("B", 1965)]);
        assert_eq!(app.latest_sol, Some(1965));

        // Narrowing to one day must not shrink the slider's range.
        app.absorb(vec![sol_image("C", 12)]);
        assert_eq!(app.latest_sol, Some(1965));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changing_filters_changes_the_cache_key() {
        let a = Filters::default();
        let b = Filters {
            sol: Some(1000),
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
