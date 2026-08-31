//! HTTP access to the Mars 2020 raw-images feed.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::model::{Image, ImageDetailResponse, ImageListResponse};
use crate::query::{ENDPOINT, Query, detail_params};

const USER_AGENT: &str = concat!("nasa-photo-viewer/", env!("CARGO_PKG_VERSION"));

/// Upper bound on pages fetched by [`Client::list_all`], so a feed that stops
/// advancing cannot spin forever.
const MAX_PAGES: u64 = 1_000;

pub struct Client {
    http: reqwest::Client,
    endpoint: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::with_endpoint(ENDPOINT)
    }

    pub fn with_endpoint(endpoint: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            endpoint: endpoint.into(),
        })
    }

    /// Fetch a single page of results.
    pub async fn list(&self, query: &Query) -> Result<ImageListResponse> {
        let params = query.to_params();
        let resp = self
            .http
            .get(&self.endpoint)
            .query(&params)
            .send()
            .await
            .context("requesting raw-images feed")?;

        let status = resp.status();
        let body = resp.text().await.context("reading feed response body")?;
        if !status.is_success() {
            bail!("raw-images feed returned HTTP {status}");
        }

        serde_json::from_str(&body).with_context(|| {
            format!(
                "decoding feed response (HTTP {status}, {} bytes): {}",
                body.len(),
                body.chars().take(200).collect::<String>()
            )
        })
    }

    /// Fetch up to `limit` images, walking pages from `query.page` onward.
    pub async fn list_all(&self, query: &Query, limit: usize) -> Result<Vec<Image>> {
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }

        for offset in 0..MAX_PAGES {
            let page = query.page + offset;
            let resp = self.list(&query.with_page(page)).await?;
            if resp.is_exhausted() {
                break;
            }

            let remaining = limit - out.len();
            let batch = resp.images;
            let short_page = batch.len() < query.effective_num() as usize;

            out.extend(batch.into_iter().take(remaining));
            if out.len() >= limit || short_page {
                break;
            }
        }

        Ok(out)
    }

    /// Look up one image by `imageid`.
    pub async fn get(&self, id: &str) -> Result<Option<Image>> {
        let params = detail_params(id);
        let resp = self
            .http
            .get(&self.endpoint)
            .query(&params)
            .send()
            .await
            .context("requesting image detail")?;

        let status = resp.status();
        let body = resp.text().await.context("reading detail response body")?;
        if !status.is_success() {
            bail!("raw-images feed returned HTTP {status}");
        }

        let detail: ImageDetailResponse =
            serde_json::from_str(&body).context("decoding detail response")?;
        Ok(detail.image.into_iter().next())
    }

    /// Download an image URL to `path`, returning the number of bytes written.
    pub async fn download(&self, url: &str, path: &Path) -> Result<u64> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("downloading {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            bail!("download of {url} returned HTTP {status}");
        }

        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading body of {url}"))?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        tokio::fs::write(path, &bytes)
            .await
            .with_context(|| format!("writing {}", path.display()))?;

        Ok(bytes.len() as u64)
    }
}
