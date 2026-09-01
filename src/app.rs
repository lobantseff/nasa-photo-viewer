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

/// Version reported by the application, resolved from git tags at build time
/// rather than from `Cargo.toml`. See `build.rs`.
pub const VERSION: &str = env!("NPV_VERSION");

const STORAGE_KEY: &str = "npv_filters";

/// Persisted separately from the filters: with a sol filter restored at
/// launch, every image loaded is from that one day, so the slider's upper
/// bound cannot be recovered from the results.
const LATEST_SOL_KEY: &str = "npv_latest_sol";

/// The newest version the user has already been told about, so dismissing one
/// release does not have to be repeated on every launch.
const UPDATE_DISMISSED_KEY: &str = "npv_update_dismissed";

/// Whether to look for new releases at all.
const UPDATE_CHECK_KEY: &str = "npv_update_check";

/// Back-navigation label.
///
/// egui's default fonts have no arrow glyphs (U+2190 and the emoji arrows all
/// render as tofu), so this uses a guillemet, which they do provide.
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

/// Width of the filter sidebar.
const SIDEBAR_WIDTH: f32 = 230.0;

/// Smallest the camera list may become in a short window, below which
/// scrolling it would be more awkward than the space it saves.
const MIN_CAMERA_LIST_HEIGHT: f32 = 120.0;

/// Width of the sol slider's track, narrowed from egui's default so the
/// "Reset" button shares its row within the sidebar.
const SOL_SLIDER_WIDTH: f32 = 76.0;

/// Lines a page-scroll stands for, on the rare device that reports pages.
const PAGE_LINES: f32 = 10.0;

const BACK_LABEL: &str = "\u{2039} Gallery";

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Filters {
    /// Newest sol to show, or `None` to start from the latest.
    ///
    /// An upper bound rather than an exact day: results run newest-first from
    /// here backwards, so the chosen sol is at the top and browsing continues
    /// into earlier ones instead of stopping at a day boundary.
    pub up_to_sol: Option<i64>,
    /// Cameras whose images are shown.
    ///
    /// Every camera is enabled by default, so the checkboxes state what is on
    /// screen rather than describing a filter that is not yet applied.
    pub enabled_cameras: Vec<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            up_to_sol: None,
            enabled_cameras: MARS2020_CAMERAS.iter().map(|c| (*c).to_string()).collect(),
        }
    }
}

impl Filters {
    /// Whether an already-loaded record satisfies these filters.
    ///
    /// Filtering is applied locally as well as upstream so that narrowing the
    /// selection hides images immediately instead of discarding them and
    /// waiting on a fresh request.
    pub fn matches(&self, image: &Image) -> bool {
        if let Some(bound) = self.up_to_sol {
            match image.sol {
                Some(sol) if sol <= bound => {}
                // A record with no sol cannot be shown to satisfy a bound.
                _ => return false,
            }
        }

        if self.all_cameras_enabled() {
            return true;
        }
        match image.camera.instrument.as_deref() {
            Some(instrument) => self.enabled_cameras.iter().any(|c| c == instrument),
            None => false,
        }
    }

    /// True when nothing is filtered out.
    pub fn all_cameras_enabled(&self) -> bool {
        self.enabled_cameras.len() == MARS2020_CAMERAS.len()
    }

    /// True when every camera is switched off, which can match nothing.
    pub fn no_cameras_enabled(&self) -> bool {
        self.enabled_cameras.is_empty()
    }

    pub fn to_query(&self) -> Query {
        Query {
            num: MAX_PAGE_SIZE,
            page: 0,
            // Always newest-first: the slider sets where "newest" starts, so a
            // second ordering control would only contradict it.
            order: Order::SolDesc,
            // Every camera enabled is the same query as no camera filter,
            // and the shorter request is the one the service answers fastest.
            cameras: if self.all_cameras_enabled() {
                Vec::new()
            } else {
                self.enabled_cameras.clone()
            },
            min_sol: None,
            max_sol: self.up_to_sol,
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
    /// Indices into `images` that satisfy the current filters.
    ///
    /// Everything fetched is retained, so narrowing a filter only has to
    /// recompute this rather than throw the records away.
    visible: Vec<usize>,
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
    about_open: bool,
    /// A release newer than this build, once one has been found.
    update_available: Option<crate::update::Available>,
    update_modal_open: bool,
    /// Newest version already dismissed; persisted.
    update_dismissed: Option<String>,
    update_check_enabled: bool,
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
        app.update_dismissed = cc
            .storage
            .and_then(|s| eframe::get_value::<Option<String>>(s, UPDATE_DISMISSED_KEY))
            .flatten();
        app.update_check_enabled = cc
            .storage
            .and_then(|s| eframe::get_value::<bool>(s, UPDATE_CHECK_KEY))
            .unwrap_or(true);
        app.start_update_check();
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
            visible: Vec::new(),
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
            about_open: false,
            update_available: None,
            update_modal_open: false,
            update_dismissed: None,
            update_check_enabled: true,
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

    /// The image at `position` in the filtered view.
    fn visible_image(&self, position: usize) -> Option<&Image> {
        self.images.get(*self.visible.get(position)?)
    }

    fn visible_len(&self) -> usize {
        self.visible.len()
    }

    fn recompute_visible(&mut self) {
        let filters = self.filters.clone();
        self.visible = self
            .images
            .iter()
            .enumerate()
            .filter(|(_, image)| filters.matches(image))
            .map(|(i, _)| i)
            .collect();
    }

    fn absorb(&mut self, images: Vec<Image>) {
        for image in images {
            // Kept monotonic: filtering to one sol would otherwise shrink the
            // slider's range to that day.
            if let Some(sol) = image.sol {
                self.latest_sol = Some(self.latest_sol.map_or(sol, |seen| seen.max(sol)));
            }
            if self.seen.insert(image.id().to_string()) {
                if self.filters.matches(&image) {
                    self.visible.push(self.images.len());
                }
                self.images.push(image);
            }
        }
    }

    fn reset_for_new_filters(&mut self) {
        self.active_key = self.filters.to_query().cache_key();
        // Records already fetched are kept and simply re-filtered: unticking a
        // camera hides its images at once instead of emptying the window while
        // the same photographs are fetched again. Textures are keyed by URL
        // and bounded by their own budget, so they are kept for the same
        // reason.
        self.recompute_visible();
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
        if self.exhausted || self.filters.no_cameras_enabled() {
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
                Update::LatestRelease(latest) => {
                    self.update_available =
                        crate::update::evaluate(VERSION, &latest, self.update_dismissed.as_deref());
                    self.update_modal_open = self.update_available.is_some();
                }
                Update::Connectivity { .. } => {}
            }
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
            self.visible.clear();
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
            .exact_size(SIDEBAR_WIDTH)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("Perseverance");
                ui.separator();

                let before = self.filters.clone();

                ui.horizontal(|ui| {
                    ui.label("Up to sol");
                    ui.label(RichText::new("(Martian day; 0 is landing)").weak().small());
                });

                match self.latest_sol {
                    Some(latest) => {
                        ui.horizontal(|ui| {
                            // Leave room for the button beside it rather than
                            // pushing it onto a row of its own.
                            ui.spacing_mut().slider_width = SOL_SLIDER_WIDTH;

                            // The slider's value box accepts typing, so an
                            // exact sol is still reachable without dragging.
                            let mut value =
                                self.filters.up_to_sol.unwrap_or(latest).clamp(0, latest);
                            let changed = ui
                                .add(egui::Slider::new(&mut value, 0..=latest).step_by(1.0))
                                .changed();
                            if changed {
                                self.filters.up_to_sol = Some(value);
                            }

                            // Always drawn, so the row does not reflow when a
                            // sol is picked or cleared.
                            let filtered = self.filters.up_to_sol.is_some();
                            if ui
                                .add_enabled(filtered, egui::Button::new("Reset"))
                                .clicked()
                            {
                                self.filters.up_to_sol = None;
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
                ui.horizontal(|ui| {
                    ui.label("Cameras");
                    let filtered = !self.filters.all_cameras_enabled();
                    if ui
                        .add_enabled(filtered, egui::Button::new("All").small())
                        .clicked()
                    {
                        self.filters.enabled_cameras =
                            MARS2020_CAMERAS.iter().map(|c| (*c).to_string()).collect();
                    }
                });

                // Claimed before the list, so the counts keep the foot of the
                // sidebar and the list is measured against what is left.
                let (total_results, shown) = (self.total_results, self.visible_len());
                egui::Panel::bottom("filter_summary")
                    .show_separator_line(false)
                    .show(ui, |ui| {
                        ui.separator();
                        match total_results {
                            Some(total) => ui.label(format!("{total} matching images")),
                            None => ui.label("Loading…"),
                        };
                        ui.label(format!("{shown} shown"));
                    });

                camera_list(ui, &mut self.filters.enabled_cameras);

                if self.filters != before {
                    self.reset_for_new_filters();
                }
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

                let error = self.error.clone();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Added first, so the version keeps the corner whatever
                    // else the status bar is reporting.
                    if ui
                        .add(
                            egui::Label::new(RichText::new(VERSION).weak())
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("About this application")
                        .clicked()
                    {
                        self.about_open = true;
                    }

                    if let Some(err) = error {
                        ui.separator();
                        if ui.small_button("dismiss").clicked() {
                            self.error = None;
                        }
                        ui.colored_label(Color32::LIGHT_RED, truncate(&err, 90));
                    }
                });
            });
        });
    }

    /// Ask GitHub for the latest release, unless there is no point.
    ///
    /// A development build is deliberately excluded: it sits after some
    /// release and may carry unreleased work, so pointing it at a download
    /// would be telling the user to discard that.
    fn start_update_check(&mut self) {
        if !crate::update::should_check(self.update_check_enabled, VERSION) {
            return;
        }
        self.fetcher.request_latest_release();
    }

    fn update_window(&mut self, ctx: &egui::Context) {
        let Some(available) = self.update_available.clone() else {
            return;
        };
        if !self.update_modal_open {
            return;
        }

        let mut open = true;
        egui::Window::new("Update available")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.heading(format!("{} is available", available.version));
                ui.add_space(4.0);
                ui.label(format!("You are running {VERSION}."));
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if ui.button("Open downloads").clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(&available.url));
                        // Opening the page is as good as being told: there is
                        // no reason to raise it again for this version.
                        self.dismiss_update();
                    }
                    if ui.button("Not now").clicked() {
                        self.dismiss_update();
                    }
                });
            });

        // Closing with the window's own control counts as dismissal too.
        if !open {
            self.dismiss_update();
        }
    }

    /// Stop offering this version, now and on future launches.
    fn dismiss_update(&mut self) {
        if let Some(available) = &self.update_available {
            self.update_dismissed = Some(available.version.clone());
        }
        self.update_modal_open = false;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }

        let mut open = true;
        egui::Window::new("About")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            // Opens centred but stays draggable. An anchor would re-pin it
            // every frame, so it could not be moved off whatever it covers.
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            // Dragging it fully off screen would leave no way to reach it.
            .constrain(true)
            .show(ctx, |ui| {
                ui.heading("NASA Photo Viewer");
                ui.label(RichText::new(VERSION).weak());
                ui.add_space(8.0);

                ui.label(
                    "A desktop browser for the raw images returned by NASA's \
                     Mars 2020 rover, Perseverance.",
                );
                ui.add_space(8.0);

                ui.separator();
                egui::Grid::new("about_details")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Images").weak());
                        ui.hyperlink_to(
                            "mars.nasa.gov/mars2020",
                            "https://mars.nasa.gov/mars2020/multimedia/raw-images/",
                        );
                        ui.end_row();

                        ui.label(RichText::new("Credit").weak());
                        ui.label("NASA/JPL-Caltech");
                        ui.end_row();

                        ui.label(RichText::new("Cache").weak());
                        ui.label(
                            crate::cache::default_cache_dir()
                                .map(|d| d.display().to_string())
                                .unwrap_or_else(|_| "unavailable".to_string()),
                        );
                        ui.end_row();
                    });

                ui.add_space(8.0);
                ui.separator();

                if ui
                    .checkbox(&mut self.update_check_enabled, "Check for updates")
                    .on_hover_text(
                        "Asks GitHub for the latest release once at startup. \
                         Nothing else is sent.",
                    )
                    .changed()
                    && self.update_check_enabled
                {
                    // Turning it back on should say something now rather than
                    // waiting for the next launch.
                    self.start_update_check();
                }

                if let Some(available) = self.update_available.clone() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} available", available.version)).weak());
                        if ui.small_button("open").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(&available.url));
                        }
                    });
                }

                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Images are public domain. This viewer is not affiliated \
                         with NASA.",
                    )
                    .weak()
                    .small(),
                );
            });

        self.about_open = open;
    }

    fn gallery(&mut self, ui: &mut egui::Ui) {
        // Switching every camera off can only match nothing, so say so rather
        // than sending a query whose empty result looks like a failure.
        if self.filters.no_cameras_enabled() {
            ui.centered_and_justified(|ui| {
                ui.label("Every camera is switched off.");
            });
            return;
        }

        if self.visible.is_empty() {
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
        let rows = self.visible_len().div_ceil(columns);

        let mut clicked = None;
        let mut wanted: Vec<String> = Vec::new();

        // show_rows only builds the visible rows, so a set of thousands of
        // images costs the same to draw as a screenful.
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show_rows(ui, cell, rows, |ui, row_range| {
                let prefetch_end = (row_range.end + PREFETCH_ROWS).min(rows) * columns;
                let visible_end = (row_range.end * columns).min(self.visible_len());

                for row in row_range.clone() {
                    ui.horizontal(|ui| {
                        for col in 0..columns {
                            let Some(idx) = index_at(row, col, columns, self.visible_len()) else {
                                break;
                            };
                            if self.thumb(ui, idx, &mut wanted) {
                                clicked = Some(idx);
                            }
                        }
                    });
                }

                // Queue thumbnails slightly beyond the viewport.
                for idx in visible_end..prefetch_end.min(self.visible_len()) {
                    if let Some(url) = self
                        .visible_image(idx)
                        .and_then(|i| i.url_for(ImageSize::Small))
                        && !self.textures.contains(url)
                    {
                        wanted.push(url.to_string());
                    }
                }

                if visible_end + PAGE_LOOKAHEAD >= self.visible_len() {
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
        // Resolved to a field borrow rather than through a method, so that
        // the texture store stays independently borrowable below.
        let Some(&position) = self.visible.get(idx) else {
            return false;
        };
        let image = &self.images[position];
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
            // A thumbnail opens the detail view, so it gets the same hand as
            // any other clickable thing.
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
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
        let Some(image) = self.visible_image(idx) else {
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

        if next < self.visible_len() {
            self.select(next);
        } else if !self.exhausted {
            // Stepping off the end is a clear request to keep going, so carry
            // the intent until the next page lands.
            self.pending_advance = true;
        }

        // Paging is otherwise driven by the gallery's scroll position, which
        // is not running while the detail view is open. Without this, browsing
        // by keyboard stops dead at the edge of the loaded batch.
        if next + PAGE_LOOKAHEAD >= self.visible_len() {
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
        if current + 1 < self.visible_len() {
            self.pending_advance = false;
            self.select(current + 1);
        }
    }

    fn detail(&mut self, ui: &mut egui::Ui, idx: usize) {
        let Some(image) = self.visible_image(idx).cloned() else {
            return;
        };

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
        eframe::set_value(storage, UPDATE_DISMISSED_KEY, &self.update_dismissed);
        eframe::set_value(storage, UPDATE_CHECK_KEY, &self.update_check_enabled);
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
        self.about_window(&ctx);
        self.update_window(&ctx);

        egui::CentralPanel::default().show(ui, |ui| match self.selected {
            Some(idx) if idx < self.visible_len() => self.detail(ui, idx),
            _ => self.gallery(ui),
        });
    }
}

/// Apply a click on `cam` to the set of enabled cameras.
///
/// A plain click toggles that one camera. An alt-click isolates it instead,
/// which saves switching off fifteen others to look at one; alt-clicking the
/// camera that is already alone inverts the selection, so the same gesture
/// both enters and leaves the isolated view.
fn apply_camera_toggle(enabled: &[String], cam: &str, alt: bool) -> Vec<String> {
    let is_alone = enabled.len() == 1 && enabled[0] == cam;

    let keep: Box<dyn Fn(&str) -> bool> = if alt && is_alone {
        Box::new(move |c| c != cam)
    } else if alt {
        Box::new(move |c| c == cam)
    } else if enabled.iter().any(|c| c == cam) {
        Box::new(move |c| c != cam && enabled.iter().any(|e| e == c))
    } else {
        Box::new(move |c| c == cam || enabled.iter().any(|e| e == c))
    };

    // Rebuilt in canonical order so the set never depends on click history.
    MARS2020_CAMERAS
        .iter()
        .filter(|c| keep(c))
        .map(|c| (*c).to_string())
        .collect()
}

/// How the camera list ended up laid out.
pub struct CameraListLayout {
    /// Height the list was given on screen.
    pub viewport_height: f32,
    /// Height the checkboxes needed.
    pub content_height: f32,
}

impl CameraListLayout {
    /// Whether the list had to scroll to show every camera.
    pub fn needs_scrolling(&self) -> bool {
        self.content_height > self.viewport_height + 0.5
    }
}

/// The scrollable list of camera checkboxes.
fn camera_list(ui: &mut egui::Ui, enabled: &mut Vec<String>) -> CameraListLayout {
    let output = egui::ScrollArea::vertical()
        // Fill the available width rather than shrinking to the widest camera
        // name, which strands the scroll bar mid-panel with dead space beside
        // it and lets the floating bar overlap the longest label.
        .auto_shrink([false, true])
        // Take the height the sidebar actually has rather than a fixed slice
        // of it, which left the list scrolling with the panel half empty
        // below it.
        .max_height(ui.available_height().max(MIN_CAMERA_LIST_HEIGHT))
        .show(ui, |ui| {
            for cam in MARS2020_CAMERAS {
                let mut on = enabled.iter().any(|c| c == cam);
                let response = ui.checkbox(&mut on, *cam);
                if response.clicked() {
                    // The checkbox has already flipped `on`; the decision is
                    // made from the stored set, so that is discarded.
                    let alt = ui.input(|i| i.modifiers.alt);
                    *enabled = apply_camera_toggle(enabled, cam, alt);
                }
                response.on_hover_text(if enabled.len() == 1 && enabled[0] == *cam {
                    "Alt-click to show every other camera"
                } else {
                    "Alt-click to show only this camera"
                });
            }
        });

    CameraListLayout {
        viewport_height: output.inner_rect.height(),
        content_height: output.content_size.y,
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

    fn camera_image(id: &str, instrument: &str) -> Image {
        serde_json::from_value(serde_json::json!({
            "imageid": id,
            "sol": 1000,
            "camera": { "instrument": instrument },
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

    /// Run until no request is outstanding.
    ///
    /// Requests against the refused port fail quickly, but how quickly is the
    /// platform's business, so tests that care about a *new* request have to
    /// wait rather than assume.
    fn drain(harness: &mut egui_kittest::Harness<'_, App>) {
        for _ in 0..400 {
            if harness.state().fetcher.inflight_count() == 0 {
                return;
            }
            settle(harness);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("requests never drained");
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

        app.filters.up_to_sol = Some(1000);
        app.reset_for_new_filters();

        // Filters usually overlap, so discarding decoded thumbnails would
        // force them all to be fetched and decoded again.
        assert_eq!(app.textures.count(Tier::Thumbnail), before);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An app holding two images from different cameras.
    fn app_with_two_cameras() -> (App, std::path::PathBuf) {
        let dir = temp_cache_dir();
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let mut app = offline_app(egui::Context::default(), cache);
        app.absorb(vec![
            camera_image("L", "NAVCAM_LEFT"),
            camera_image("R", "NAVCAM_RIGHT"),
        ]);
        (app, dir)
    }

    #[test]
    fn unticking_a_camera_hides_its_images_without_discarding_them() {
        let (mut app, dir) = app_with_two_cameras();
        assert_eq!(app.visible_len(), 2);

        app.filters.enabled_cameras =
            apply_camera_toggle(&app.filters.enabled_cameras, "NAVCAM_LEFT", false);
        app.reset_for_new_filters();

        // Hidden from the gallery...
        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.visible_image(0).unwrap().id(), "R");
        // ...but still held, so re-ticking cannot need a fresh request.
        assert_eq!(app.images.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reticking_a_camera_restores_its_images_from_memory() {
        let (mut app, dir) = app_with_two_cameras();

        app.filters.enabled_cameras =
            apply_camera_toggle(&app.filters.enabled_cameras, "NAVCAM_LEFT", false);
        app.reset_for_new_filters();
        let after_hiding = app.fetcher.issued_count();

        app.filters.enabled_cameras =
            apply_camera_toggle(&app.filters.enabled_cameras, "NAVCAM_LEFT", false);
        app.reset_for_new_filters();

        assert_eq!(
            app.visible_len(),
            2,
            "the hidden image should come straight back"
        );
        let _ = after_hiding;

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn narrowing_the_filter_keeps_the_gallery_populated() {
        let (mut app, dir) = app_with_two_cameras();

        app.filters.enabled_cameras = vec!["NAVCAM_RIGHT".to_string()];
        app.reset_for_new_filters();

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        // The remaining image was already loaded, so the window must not fall
        // back to the loading panel while the narrowed query is fetched.
        assert!(
            harness.query_by_label("Loading images\u{2026}").is_none(),
            "already-loaded images should stay on screen while refetching"
        );
        assert!(harness.query_by_label("R").is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_sol_bound_also_filters_loaded_images() {
        let dir = temp_cache_dir();
        let cache = Cache::open_at(&dir, DEFAULT_CACHE_BUDGET).unwrap();
        let mut app = offline_app(egui::Context::default(), cache);
        app.absorb(vec![sol_image("OLD", 900), sol_image("NEW", 1900)]);
        assert_eq!(app.visible_len(), 2);

        app.filters.up_to_sol = Some(1000);
        app.reset_for_new_filters();

        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.visible_image(0).unwrap().id(), "OLD");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn images_arriving_later_respect_the_current_filter() {
        let (mut app, dir) = app_with_two_cameras();
        app.filters.enabled_cameras = vec!["NAVCAM_RIGHT".to_string()];
        app.reset_for_new_filters();

        // A page still in flight for the previous filter must not reintroduce
        // images the user has just hidden.
        app.absorb(vec![camera_image("L2", "NAVCAM_LEFT")]);

        assert_eq!(app.visible_len(), 1);
        assert!(
            app.images.len() == 3,
            "the record is kept, merely not shown"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_camera_starts_enabled() {
        let f = Filters::default();

        // The checkboxes describe what is on screen, so with everything shown
        // they must all be ticked.
        assert_eq!(f.enabled_cameras.len(), MARS2020_CAMERAS.len());
        assert!(f.all_cameras_enabled());
        // All enabled is the same query as no camera filter.
        assert!(f.to_query().cameras.is_empty());
    }

    #[test]
    fn unticking_a_camera_filters_it_out() {
        let all: Vec<String> = MARS2020_CAMERAS.iter().map(|c| c.to_string()).collect();

        let without = apply_camera_toggle(&all, "SKYCAM", false);

        assert_eq!(without.len(), MARS2020_CAMERAS.len() - 1);
        assert!(!without.iter().any(|c| c == "SKYCAM"));

        // And ticking it again restores it.
        let restored = apply_camera_toggle(&without, "SKYCAM", false);
        assert_eq!(restored, all);
    }

    #[test]
    fn alt_click_isolates_a_single_camera() {
        let all: Vec<String> = MARS2020_CAMERAS.iter().map(|c| c.to_string()).collect();

        let only = apply_camera_toggle(&all, "NAVCAM_LEFT", true);

        assert_eq!(only, vec!["NAVCAM_LEFT".to_string()]);
    }

    #[test]
    fn alt_click_on_an_isolated_camera_inverts_the_selection() {
        let only = vec!["NAVCAM_LEFT".to_string()];

        let inverted = apply_camera_toggle(&only, "NAVCAM_LEFT", true);

        // The same gesture leaves the isolated view as entered it.
        assert_eq!(inverted.len(), MARS2020_CAMERAS.len() - 1);
        assert!(!inverted.iter().any(|c| c == "NAVCAM_LEFT"));
    }

    #[test]
    fn alt_click_on_another_camera_moves_the_isolation() {
        let only = vec!["NAVCAM_LEFT".to_string()];

        // Not the isolated one, so this isolates rather than inverting.
        let moved = apply_camera_toggle(&only, "MCZ_RIGHT", true);

        assert_eq!(moved, vec!["MCZ_RIGHT".to_string()]);
    }

    #[test]
    fn the_enabled_set_keeps_a_canonical_order() {
        let scrambled = vec!["SKYCAM".to_string(), "NAVCAM_LEFT".to_string()];

        let toggled = apply_camera_toggle(&scrambled, "MCZ_LEFT", false);

        // Order follows the camera list, not the order things were clicked.
        let expected: Vec<String> = MARS2020_CAMERAS
            .iter()
            .filter(|c| ["NAVCAM_LEFT", "MCZ_LEFT", "SKYCAM"].contains(c))
            .map(|c| c.to_string())
            .collect();
        assert_eq!(toggled, expected);
    }

    #[test]
    fn unticking_the_last_camera_is_allowed_but_queries_nothing() {
        let one = vec!["SKYCAM".to_string()];

        let none = apply_camera_toggle(&one, "SKYCAM", false);

        assert!(none.is_empty());
        let f = Filters {
            enabled_cameras: none,
            ..Filters::default()
        };
        assert!(f.no_cameras_enabled());
    }

    #[test]
    fn no_request_is_made_when_every_camera_is_off() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.exhausted = false;
        app.filters.enabled_cameras.clear();

        let before = app.fetcher.issued_count();
        app.request_more();

        // Such a query can only come back empty, so it is not worth sending.
        assert_eq!(app.fetcher.issued_count(), before);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn alt_click_reaches_the_camera_list_through_the_ui() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        assert!(harness.state().filters.all_cameras_enabled());

        // The modifier has to survive the trip through the checkbox, which
        // flips its own bool before the handler sees the click.
        harness
            .get_by_label("MCZ_LEFT")
            .click_modifiers(egui::Modifiers::ALT);
        settle(&mut harness);

        assert_eq!(
            harness.state().filters.enabled_cameras,
            vec!["MCZ_LEFT".to_string()],
            "alt-click should isolate the camera, not merely untick it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_plain_click_only_unticks_one_camera() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        harness.get_by_label("MCZ_LEFT").click();
        settle(&mut harness);

        let enabled = &harness.state().filters.enabled_cameras;
        assert_eq!(enabled.len(), MARS2020_CAMERAS.len() - 1);
        assert!(!enabled.iter().any(|c| c == "MCZ_LEFT"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_camera_list_uses_the_height_it_is_given() {
        let ctx = egui::Context::default();
        let mut cameras: Vec<String> = Vec::new();

        // Ample room: every camera should be reachable without scrolling,
        // rather than the list keeping to a fixed slice of the sidebar and
        // leaving the rest of the panel empty.
        let mut roomy = None;
        let mut cramped = None;
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.allocate_ui(egui::vec2(230.0, 600.0), |ui| {
                roomy = Some(camera_list(ui, &mut cameras));
            });
            ui.allocate_ui(egui::vec2(230.0, 150.0), |ui| {
                cramped = Some(camera_list(ui, &mut cameras));
            });
        });
        output.textures_delta.clear();

        let roomy = roomy.unwrap();
        assert!(
            !roomy.needs_scrolling(),
            "list still scrolled with {} of space for {} of cameras",
            roomy.viewport_height,
            roomy.content_height
        );

        // And it still yields when the window genuinely is short.
        let cramped = cramped.unwrap();
        assert!(cramped.needs_scrolling());
        assert!(cramped.viewport_height <= roomy.viewport_height);
    }

    #[test]
    fn the_camera_list_claims_the_full_width_available_to_it() {
        // The camera names are narrower than the panel, so a scroll area left
        // to shrink to its content claims only that much width and leaves its
        // scroll bar floating well inside the panel edge.
        let ctx = egui::Context::default();
        let mut cameras = Vec::new();
        let (mut available, mut claimed) = (0.0_f32, 0.0_f32);

        let mut output = ctx.run_ui(Default::default(), |ui| {
            available = ui.available_width();
            camera_list(ui, &mut cameras);
            claimed = ui.min_rect().width();
        });
        output.textures_delta.clear();

        assert!(
            claimed >= available - 1.0,
            "camera list claimed {claimed} of {available} available"
        );
    }

    #[test]
    fn the_sol_control_and_its_button_share_one_row_inside_the_sidebar() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.latest_sol = Some(1965);
        app.filters.up_to_sol = Some(1000);

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        let button = harness.get_by_label("Reset").rect();

        // On its own row the button would start at the sidebar's left margin.
        assert!(
            button.min.x > SOL_SLIDER_WIDTH,
            "button wrapped onto its own row, starting at x={}",
            button.min.x
        );
        // And it must not be pushed out past the sidebar to achieve that.
        assert!(
            button.max.x <= SIDEBAR_WIDTH,
            "button overflows the sidebar: ends at x={} of {SIDEBAR_WIDTH}",
            button.max.x
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn latest(tag: &str) -> crate::update::LatestRelease {
        crate::update::LatestRelease {
            tag_name: tag.to_string(),
            html_url: Some(format!("https://example.invalid/{tag}")),
        }
    }

    /// Put the app in the state it reaches after finding a newer release.
    fn app_offered(tag: &str) -> (App, std::path::PathBuf) {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.update_available = Some(crate::update::Available {
            version: tag.to_string(),
            url: format!("https://example.invalid/{tag}"),
        });
        app.update_modal_open = true;
        (app, dir)
    }

    #[test]
    fn the_update_notice_appears_and_can_be_dismissed() {
        let (app, dir) = app_offered("v9.9.9");
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        assert!(harness.query_by_label("Open downloads").is_some());

        harness.get_by_label("Not now").click();
        settle(&mut harness);

        assert!(!harness.state().update_modal_open);
        // Remembered, so the next launch does not repeat itself.
        assert_eq!(harness.state().update_dismissed.as_deref(), Some("v9.9.9"));
        assert!(harness.query_by_label("Open downloads").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opening_the_downloads_page_also_dismisses_the_notice() {
        let (app, dir) = app_offered("v9.9.9");
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        harness.get_by_label("Open downloads").click();
        settle(&mut harness);

        // Having been sent to the page, being asked again would be nagging.
        assert_eq!(harness.state().update_dismissed.as_deref(), Some("v9.9.9"));
        assert!(!harness.state().update_modal_open);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_notice_appears_when_there_is_no_newer_release() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        assert!(harness.query_by_label("Open downloads").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_release_report_is_judged_against_the_dismissed_version() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.update_dismissed = Some("v9.9.9".to_string());

        // Already dismissed: nothing to say.
        app.update_available =
            crate::update::evaluate(VERSION, &latest("v9.9.9"), app.update_dismissed.as_deref());
        assert!(app.update_available.is_none());

        // A later one still gets through.
        app.update_available =
            crate::update::evaluate(VERSION, &latest("v99.0.0"), app.update_dismissed.as_deref());
        // Only meaningful for a release build; a dev build declines either way.
        if crate::update::is_release_build(VERSION) {
            assert!(app.update_available.is_some());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switching_the_check_off_stops_it_asking() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.update_check_enabled = false;

        let before = app.fetcher.issued_count();
        app.start_update_check();

        assert_eq!(
            app.fetcher.issued_count(),
            before,
            "no request should be made"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_check_follows_the_rule_for_this_build() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.update_check_enabled = true;

        let before = app.fetcher.issued_count();
        app.start_update_check();
        let issued = app.fetcher.issued_count() - before;

        // Whether this build asks is decided by should_check, which is tested
        // exhaustively of its own accord; here it only has to be obeyed.
        let expected = u64::from(crate::update::should_check(true, VERSION));
        assert_eq!(issued, expected);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_version_is_resolved_from_git_at_build_time() {
        // Rejects the placeholder a failed `git describe` falls back to, which
        // is otherwise indistinguishable from a real version at a glance.
        assert!(
            crate::version_format::is_well_formed(VERSION),
            "build.rs produced {VERSION:?}, which names no commit"
        );
    }

    #[test]
    fn the_status_bar_shows_the_version_and_no_longer_reports_full_resolution() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        assert!(harness.query_by_label(VERSION).is_some());
        for gone in ["full resolution", "loading full resolution"] {
            assert!(
                harness.query_by_label(gone).is_none(),
                "{gone:?} should have been removed from the status bar"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hovering_the_version_shows_a_pointing_hand() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        let version = harness.get_by_label(VERSION).rect();
        harness.hover_at(version.center());
        settle(&mut harness);

        // The version opens the About window, so it has to look clickable.
        assert_eq!(
            harness.output().platform_output.cursor_icon,
            egui::CursorIcon::PointingHand
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hovering_a_thumbnail_shows_a_pointing_hand() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        let thumb = harness.get_by_label("A").rect();
        harness.hover_at(thumb.center());
        settle(&mut harness);

        assert_eq!(
            harness.output().platform_output.cursor_icon,
            egui::CursorIcon::PointingHand
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_space_keeps_the_ordinary_cursor() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        // Guards against the hand being set unconditionally rather than only
        // over something that responds to a click.
        harness.hover_at(egui::pos2(1.0, 1.0));
        settle(&mut harness);

        assert_ne!(
            harness.output().platform_output.cursor_icon,
            egui::CursorIcon::PointingHand
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clicking_the_version_opens_the_about_window() {
        let (app, dir) = test_app_thumbs_only(&["A"]);
        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        assert!(!harness.state().about_open);
        assert!(harness.query_by_label("NASA Photo Viewer").is_none());

        harness.get_by_label(VERSION).click();
        settle(&mut harness);

        assert!(harness.state().about_open);
        // The window states what the application is and where its images
        // come from, which is the whole point of opening it.
        assert!(harness.query_by_label("NASA Photo Viewer").is_some());
        assert!(harness.query_by_label("NASA/JPL-Caltech").is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_about_window_can_be_dragged_somewhere_else() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.about_open = true;

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);

        let before = harness.get_by_label("NASA Photo Viewer").rect();

        // Grab the title bar and move it. An anchored window would be pinned
        // back to the same place every frame, so nothing would change.
        //
        // Done by hand rather than with drag_at/drop_at, which only press and
        // release: egui derives the drag from the movement in between, so
        // without a PointerMoved the window never travels.
        // The accessible node covers the whole window, and by default only the
        // title bar drags it, so aim near the top edge rather than the middle.
        let window = harness.get_by_label("About").rect();
        let from = egui::pos2(window.center().x, window.min.y + 10.0);
        let to = from + egui::vec2(-120.0, -60.0);

        harness.event(egui::Event::PointerButton {
            pos: from,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        settle(&mut harness);
        harness.event(egui::Event::PointerMoved(to));
        settle(&mut harness);
        harness.event(egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        settle(&mut harness);

        let after = harness.get_by_label("NASA Photo Viewer").rect();
        assert_ne!(
            before.min, after.min,
            "the About window did not move when dragged"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_about_window_can_be_closed_again() {
        let (mut app, dir) = test_app_thumbs_only(&["A"]);
        app.about_open = true;

        let mut harness =
            egui_kittest::Harness::new_ui_state(|ui, app: &mut App| app.ui_impl(ui), app);
        settle(&mut harness);
        assert!(harness.query_by_label("NASA Photo Viewer").is_some());

        harness.state_mut().about_open = false;
        settle(&mut harness);

        assert!(harness.query_by_label("NASA Photo Viewer").is_none());

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

        // Let the gallery's own page request finish first. Requests are
        // de-duplicated by key, so an identical one still in flight would
        // absorb the press being tested and the count would not move. How
        // long that takes depends on how quickly the platform refuses the
        // connection, which is why this cannot be assumed.
        drain(&mut harness);

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
            "fetching more results…",
            "Gallery",
            "Clear",
            "reset",
            "dismiss",
            "online",
            "offline",
            "Perseverance",
            "Up to sol",
            "(Martian day; 0 is landing)",
            "Reset",
            "waiting for the first results",
            "Cameras",
            "Update available",
            "Open downloads",
            "Not now",
            "Check for updates",
            "About",
            "NASA Photo Viewer",
            "About this application",
            "NASA/JPL-Caltech",
            "Credit",
            "Images",
            "Cache",
            "All",
            "Every camera is switched off.",
            "Alt-click to show only this camera",
            "Alt-click to show every other camera",
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
    fn the_slider_sets_an_upper_bound_not_an_exact_day() {
        let f = Filters {
            up_to_sol: Some(1000),
            enabled_cameras: vec!["NAVCAM_LEFT".into()],
        };
        let q = f.to_query();

        // An upper bound only: results run back from sol 1000, so browsing
        // carries on into earlier sols instead of stopping at that day.
        assert_eq!(q.max_sol, Some(1000));
        assert_eq!(q.min_sol, None);
        assert_eq!(q.order, Order::SolDesc);
        assert_eq!(q.cameras, vec!["NAVCAM_LEFT".to_string()]);
    }

    #[test]
    fn no_sol_selected_starts_from_the_latest() {
        let q = Filters::default().to_query();

        assert_eq!(q.min_sol, None);
        assert_eq!(q.max_sol, None);
        assert_eq!(q.order, Order::SolDesc);
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
            up_to_sol: Some(1000),
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
