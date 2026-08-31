//! Query construction for the raw-images feed.
//!
//! The feed expresses range filters through numbered `condition_N` slots.
//! Slot 1 is reserved for the mission scope; range filters occupy slots 2 and
//! up. A range placed in `condition_1` is silently ignored — the request still
//! returns HTTP 200 with an unfiltered result set — so slot allocation is
//! enforced here rather than left to callers.

/// Maximum number of records the feed will return per page. Larger values are
/// accepted but silently capped by the server.
pub const MAX_PAGE_SIZE: u32 = 100;

pub const ENDPOINT: &str = "https://mars.nasa.gov/rss/api/";

/// This feed serves Perseverance only.
///
/// `category=msl` is accepted and answers HTTP 200, but always with zero
/// results and `"error_message": "No more images."`. Curiosity's raw images
/// live behind a different service (`/api/v1/raw_image_items/`) with an
/// incompatible schema, so this client does not offer a misleading toggle.
pub const CATEGORY: &str = "mars2020";

/// Camera instrument names accepted by the `search` parameter, verified
/// against live result counts.
pub const MARS2020_CAMERAS: &[&str] = &[
    "NAVCAM_LEFT",
    "NAVCAM_RIGHT",
    "FRONT_HAZCAM_LEFT_A",
    "FRONT_HAZCAM_RIGHT_A",
    "REAR_HAZCAM_LEFT",
    "REAR_HAZCAM_RIGHT",
    "MCZ_LEFT",
    "MCZ_RIGHT",
    "SHERLOC_WATSON",
    "SUPERCAM_RMI",
    "PIXL_MCC",
    "SKYCAM",
    "EDL_RUCAM",
    "EDL_RDCAM",
    "EDL_DDCAM",
    "EDL_PUCAM1",
    "EDL_PUCAM2",
    "LCAM",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Order {
    /// Newest sol first.
    #[default]
    SolDesc,
    SolAsc,
    DateTakenDesc,
}

impl Order {
    pub fn as_str(self) -> &'static str {
        match self {
            Order::SolDesc => "sol desc",
            Order::SolAsc => "sol asc",
            Order::DateTakenDesc => "date_taken desc",
        }
    }
}

/// A list query against the raw-images feed.
#[derive(Debug, Clone)]
pub struct Query {
    pub num: u32,
    pub page: u64,
    pub order: Order,
    /// Camera instruments to match; empty means all cameras.
    ///
    /// Multiple cameras are OR-ed with `|`. A comma-separated list is accepted
    /// by the server but matches nothing.
    pub cameras: Vec<String>,
    pub min_sol: Option<i64>,
    pub max_sol: Option<i64>,
    /// Inclusive lower bound on capture date, as `YYYY-MM-DD`.
    pub taken_after: Option<String>,
    /// Inclusive upper bound on capture date, as `YYYY-MM-DD`.
    pub taken_before: Option<String>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            num: 25,
            page: 0,
            order: Order::default(),
            cameras: Vec::new(),
            min_sol: None,
            max_sol: None,
            taken_after: None,
            taken_before: None,
        }
    }
}

impl Query {
    /// Page size actually requested, after clamping to [`MAX_PAGE_SIZE`].
    pub fn effective_num(&self) -> u32 {
        self.num.clamp(1, MAX_PAGE_SIZE)
    }

    pub fn with_page(&self, page: u64) -> Self {
        Self {
            page,
            ..self.clone()
        }
    }

    /// Query-string parameters, in stable order.
    pub fn to_params(&self) -> Vec<(String, String)> {
        let mut params = vec![
            ("feed".to_string(), "raw_images".to_string()),
            ("category".to_string(), CATEGORY.to_string()),
            ("feedtype".to_string(), "json".to_string()),
            ("num".to_string(), self.effective_num().to_string()),
            ("page".to_string(), self.page.to_string()),
            ("order".to_string(), self.order.as_str().to_string()),
        ];

        if !self.cameras.is_empty() {
            params.push(("search".to_string(), self.cameras.join("|")));
        }

        // Slot 1 scopes the query to the mission; ranges start at slot 2.
        params.push(("condition_1".to_string(), format!("{CATEGORY}:mission")));

        let mut slot = 2;
        let mut push_condition = |params: &mut Vec<(String, String)>, value: String| {
            params.push((format!("condition_{slot}"), value));
            slot += 1;
        };

        if let Some(min) = self.min_sol {
            push_condition(&mut params, format!("{min}:sol:gte"));
        }
        if let Some(max) = self.max_sol {
            push_condition(&mut params, format!("{max}:sol:lte"));
        }
        if let Some(after) = &self.taken_after {
            push_condition(&mut params, format!("{after}:date_taken:gte"));
        }
        if let Some(before) = &self.taken_before {
            push_condition(&mut params, format!("{before}:date_taken:lte"));
        }

        params
    }
}

/// Parameters for a single-image lookup by `imageid`.
pub fn detail_params(id: &str) -> Vec<(String, String)> {
    vec![
        ("feed".to_string(), "raw_images".to_string()),
        ("category".to_string(), CATEGORY.to_string()),
        ("feedtype".to_string(), "json".to_string()),
        ("id".to_string(), id.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(params: &[(String, String)], key: &str) -> Option<String> {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn builds_a_default_query() {
        let params = Query::default().to_params();

        assert_eq!(lookup(&params, "feed").as_deref(), Some("raw_images"));
        assert_eq!(lookup(&params, "category").as_deref(), Some("mars2020"));
        assert_eq!(lookup(&params, "feedtype").as_deref(), Some("json"));
        assert_eq!(lookup(&params, "order").as_deref(), Some("sol desc"));
        assert_eq!(lookup(&params, "page").as_deref(), Some("0"));
        assert_eq!(lookup(&params, "search"), None);
    }

    #[test]
    fn clamps_page_size_to_the_server_cap() {
        // The server silently caps oversized pages; mirror that locally so
        // pagination arithmetic stays correct.
        let q = Query {
            num: 5000,
            ..Query::default()
        };
        assert_eq!(q.effective_num(), MAX_PAGE_SIZE);
        assert_eq!(lookup(&q.to_params(), "num").as_deref(), Some("100"));

        let q = Query {
            num: 0,
            ..Query::default()
        };
        assert_eq!(q.effective_num(), 1);
    }

    #[test]
    fn reserves_condition_1_for_the_mission_scope() {
        let q = Query {
            min_sol: Some(100),
            max_sol: Some(101),
            ..Query::default()
        };
        let params = q.to_params();

        assert_eq!(
            lookup(&params, "condition_1").as_deref(),
            Some("mars2020:mission")
        );
        assert_eq!(
            lookup(&params, "condition_2").as_deref(),
            Some("100:sol:gte")
        );
        assert_eq!(
            lookup(&params, "condition_3").as_deref(),
            Some("101:sol:lte")
        );
    }

    #[test]
    fn allocates_condition_slots_without_gaps() {
        // Only an upper bound: it must land in slot 2, not slot 3.
        let q = Query {
            max_sol: Some(50),
            ..Query::default()
        };
        let params = q.to_params();

        assert_eq!(
            lookup(&params, "condition_2").as_deref(),
            Some("50:sol:lte")
        );
        assert_eq!(lookup(&params, "condition_3"), None);
    }

    #[test]
    fn combines_sol_and_date_conditions_in_distinct_slots() {
        let q = Query {
            min_sol: Some(10),
            taken_after: Some("2026-08-01".to_string()),
            taken_before: Some("2026-08-05".to_string()),
            ..Query::default()
        };
        let params = q.to_params();

        assert_eq!(
            lookup(&params, "condition_2").as_deref(),
            Some("10:sol:gte")
        );
        assert_eq!(
            lookup(&params, "condition_3").as_deref(),
            Some("2026-08-01:date_taken:gte")
        );
        assert_eq!(
            lookup(&params, "condition_4").as_deref(),
            Some("2026-08-05:date_taken:lte")
        );
    }

    #[test]
    fn joins_multiple_cameras_with_a_pipe() {
        // A comma separator is accepted by the server but matches nothing.
        let q = Query {
            cameras: vec!["NAVCAM_LEFT".into(), "MCZ_RIGHT".into()],
            ..Query::default()
        };

        assert_eq!(
            lookup(&q.to_params(), "search").as_deref(),
            Some("NAVCAM_LEFT|MCZ_RIGHT")
        );
    }

    #[test]
    fn detail_params_use_the_id_parameter() {
        let params = detail_params("ABC_123");

        assert_eq!(lookup(&params, "id").as_deref(), Some("ABC_123"));
        assert_eq!(lookup(&params, "category").as_deref(), Some("mars2020"));
    }
}
