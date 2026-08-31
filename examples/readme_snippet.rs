use nasa_photo_viewer::{Client, ImageSize, Query};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::new()?;
    let query = Query {
        min_sol: Some(1000),
        max_sol: Some(1000),
        cameras: vec!["NAVCAM_LEFT".into()],
        ..Query::default()
    };

    for image in client.list_all(&query, 50).await? {
        println!("{} {:?}", image.id(), image.url_for(ImageSize::Large));
    }
    Ok(())
}
