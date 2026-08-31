//! CLI for browsing Perseverance raw images.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use nasa_photo_viewer::model::ImageSize;
use nasa_photo_viewer::query::{MARS2020_CAMERAS, MAX_PAGE_SIZE, Order, Query};
use nasa_photo_viewer::{Client, Image};

#[derive(Parser)]
#[command(
    name = "nasa-photo-viewer",
    about = "Browse NASA Mars 2020 (Perseverance) raw images",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List raw images matching the given filters.
    List {
        #[command(flatten)]
        filters: Filters,
        /// Emit raw JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show a single image by its image id.
    Get {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Download matching images to a directory.
    Download {
        #[command(flatten)]
        filters: Filters,
        /// Rendition to download.
        #[arg(long, value_enum, default_value_t = ImageSize::Large)]
        size: ImageSize,
        #[arg(long, short, default_value = "downloads")]
        out: PathBuf,
    },
    /// List the camera names accepted by --camera.
    Cameras,
}

#[derive(Args, Clone)]
struct Filters {
    /// Exact sol; shorthand for --min-sol N --max-sol N.
    #[arg(long, conflicts_with_all = ["min_sol", "max_sol"])]
    sol: Option<i64>,
    #[arg(long)]
    min_sol: Option<i64>,
    #[arg(long)]
    max_sol: Option<i64>,
    /// Camera instrument; repeatable, matched as OR.
    #[arg(long, short)]
    camera: Vec<String>,
    /// Earliest capture date, as YYYY-MM-DD.
    #[arg(long)]
    after: Option<String>,
    /// Latest capture date, as YYYY-MM-DD.
    #[arg(long)]
    before: Option<String>,
    #[arg(long, value_enum, default_value_t = Order::SolDesc)]
    order: Order,
    /// Maximum number of images to return, across pages.
    #[arg(long, short, default_value_t = 25)]
    limit: usize,
}

impl Filters {
    fn validate(&self) -> Result<()> {
        for cam in &self.camera {
            if !MARS2020_CAMERAS.iter().any(|k| k.eq_ignore_ascii_case(cam)) {
                bail!(
                    "unknown camera {cam:?}; run `nasa-photo-viewer cameras` for the valid names"
                );
            }
        }
        if let (Some(min), Some(max)) = (self.min_sol, self.max_sol)
            && min > max
        {
            bail!("--min-sol {min} is greater than --max-sol {max}");
        }
        if self.limit == 0 {
            bail!("--limit must be at least 1");
        }
        Ok(())
    }

    fn to_query(&self) -> Query {
        Query {
            // One request per page; the server caps pages at MAX_PAGE_SIZE.
            num: (self.limit as u32).min(MAX_PAGE_SIZE),
            page: 0,
            order: self.order,
            cameras: self.camera.iter().map(|c| c.to_uppercase()).collect(),
            min_sol: self.sol.or(self.min_sol),
            max_sol: self.sol.or(self.max_sol),
            taken_after: self.after.clone(),
            taken_before: self.before.clone(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = Client::new()?;

    match cli.command {
        Command::Cameras => {
            for cam in MARS2020_CAMERAS {
                println!("{cam}");
            }
        }

        Command::List { filters, json } => {
            filters.validate()?;
            let query = filters.to_query();

            if json {
                let resp = client.list(&query).await?;
                println!("{}", serde_json::to_string_pretty(&resp.images)?);
                return Ok(());
            }

            let head = client.list(&query).await?;
            if let Some(total) = head.total_results {
                eprintln!("{total} image(s) match; showing up to {}", filters.limit);
            }

            let images = client.list_all(&query, filters.limit).await?;
            if images.is_empty() {
                eprintln!("No images matched.");
                return Ok(());
            }
            for img in &images {
                print_row(img);
            }
        }

        Command::Get { id, json } => {
            let Some(img) = client.get(&id).await? else {
                bail!("no image found with id {id:?}");
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&img)?);
            } else {
                print_detail(&img);
            }
        }

        Command::Download { filters, size, out } => {
            filters.validate()?;
            let images = client.list_all(&filters.to_query(), filters.limit).await?;
            if images.is_empty() {
                eprintln!("No images matched.");
                return Ok(());
            }

            let mut written = 0usize;
            for img in &images {
                let Some(url) = img.url_for(size) else {
                    eprintln!("skipping {}: no image file available", img.id());
                    continue;
                };
                let path = out.join(file_name_for(img, url));
                let bytes = client
                    .download(url, &path)
                    .await
                    .with_context(|| format!("downloading image {}", img.id()))?;
                println!("{} ({} KiB)", path.display(), bytes / 1024);
                written += 1;
            }
            eprintln!("Downloaded {written} of {} image(s).", images.len());
        }
    }

    Ok(())
}

fn print_row(img: &Image) {
    println!(
        "sol {:>5}  {:<20}  {:<24}  {}",
        img.sol.map(|s| s.to_string()).unwrap_or_else(|| "?".into()),
        img.camera.instrument_or_unknown(),
        img.date_taken_utc.as_deref().unwrap_or("-"),
        img.id(),
    );
}

fn print_detail(img: &Image) {
    println!("id:         {}", img.id());
    println!("title:      {}", img.title.as_deref().unwrap_or("-"));
    println!(
        "sol:        {}",
        img.sol.map(|s| s.to_string()).unwrap_or_else(|| "-".into())
    );
    println!("camera:     {}", img.camera.instrument_or_unknown());
    println!(
        "taken utc:  {}",
        img.date_taken_utc.as_deref().unwrap_or("-")
    );
    println!(
        "taken mars: {}",
        img.date_taken_mars.as_deref().unwrap_or("-")
    );
    println!(
        "dimension:  {}",
        img.extended.dimension.as_deref().unwrap_or("-")
    );
    println!("credit:     {}", img.credit.as_deref().unwrap_or("-"));
    println!("page:       {}", img.link.as_deref().unwrap_or("-"));
    for (label, size) in [
        ("small", ImageSize::Small),
        ("medium", ImageSize::Medium),
        ("large", ImageSize::Large),
        ("full_res", ImageSize::FullRes),
    ] {
        if let Some(url) = img.url_for(size) {
            println!("{label:<11} {url}");
        }
    }
    if let Some(caption) = &img.caption {
        println!("\n{caption}");
    }
}

/// Name downloads after the stable image id, keeping the source extension.
fn file_name_for(img: &Image, url: &str) -> String {
    let ext = url
        .rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, e)| e)
        .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("jpg");
    format!("{}.{ext}", img.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_downloads_after_the_image_id() {
        let img: Image = serde_json::from_str(r#"{"imageid":"NRF_1965_X"}"#).unwrap();

        assert_eq!(
            file_name_for(&img, "https://x/y/a_1200.jpg"),
            "NRF_1965_X.jpg"
        );
        assert_eq!(file_name_for(&img, "https://x/y/a.png"), "NRF_1965_X.png");
        assert_eq!(file_name_for(&img, "https://x/y/noext"), "NRF_1965_X.jpg");
    }

    #[test]
    fn sol_shorthand_bounds_both_ends() {
        let filters = Filters {
            sol: Some(1000),
            min_sol: None,
            max_sol: None,
            camera: vec!["navcam_left".into()],
            after: None,
            before: None,
            order: Order::SolDesc,
            limit: 25,
        };
        let q = filters.to_query();

        assert_eq!(q.min_sol, Some(1000));
        assert_eq!(q.max_sol, Some(1000));
        // Camera names are normalised to the upper-case form the feed expects.
        assert_eq!(q.cameras, vec!["NAVCAM_LEFT".to_string()]);
    }

    #[test]
    fn rejects_unknown_cameras_and_inverted_ranges() {
        let base = Filters {
            sol: None,
            min_sol: None,
            max_sol: None,
            camera: vec![],
            after: None,
            before: None,
            order: Order::SolDesc,
            limit: 25,
        };

        assert!(base.validate().is_ok());
        assert!(
            Filters {
                camera: vec!["NOPE".into()],
                ..base.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Filters {
                min_sol: Some(10),
                max_sol: Some(5),
                ..base.clone()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn verifies_the_cli_definition() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
