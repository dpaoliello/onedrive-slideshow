use crate::http::{AppendPaths, Client};
use anyhow::{anyhow, Context, Result};
use egui::ColorImage;
use rand::Rng;
use reqwest::Url;
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf};
use tokio::sync::mpsc::unbounded_channel;

pub struct ImageLoader {
    client: Client,
    base_url: Url,
    config_url: Url,
    cache_directory: PathBuf,
}

#[derive(Deserialize)]
struct DriveResponse {
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    value: Vec<DriveItem>,
}

#[derive(Deserialize)]
struct DriveItem {
    id: String,
}

#[derive(Deserialize)]
struct ThumbnailResponse {
    value: Vec<HashMap<String, ThumbnailItem>>,
}

#[derive(Deserialize)]
struct ThumbnailItem {
    url: String,
}

#[derive(Deserialize)]
struct Config {
    directories: Vec<String>,
    interval: u64,
}

impl ImageLoader {
    pub fn new(base_url: &str, cache_directory: PathBuf) -> Self {
        let base_url = Url::parse(base_url).unwrap();
        Self {
            client: Client::new(),
            config_url: base_url.append_paths(&["root:", "slideshow.txt:", "content"]),
            base_url,
            cache_directory,
        }
    }

    async fn get_all_ids(&self, token: &str, first_url: Url) -> Result<Vec<DriveItem>> {
        let response = self
            .client
            .get::<DriveResponse>(token, first_url)
            .await
            .with_context(|| "Get all items")?;
        let mut items = response.value;
        let mut next_url = response.next_link;
        while let Some(url) = next_url {
            let response = self
                .client
                .get::<DriveResponse>(
                    token,
                    Url::parse(&url).with_context(|| "Next link invalid")?,
                )
                .await
                .with_context(|| "Get all items - next link")?;
            next_url = response.next_link;
            items.extend(response.value);
        }
        Ok(items)
    }

    pub async fn get_image_list(&self, token: &str) -> Result<(Vec<String>, u64)> {
        let config = self
            .client
            .get::<Config>(token, self.config_url.clone())
            .await
            .with_context(|| "Get slideshow.txt")?;

        let (image_sender, mut image_receiver) = unbounded_channel();
        let (directory_sender, mut directory_receiver) = unbounded_channel();

        let process_directory = |directory: String| {
            let mut paths = directory.split('/').collect::<Vec<_>>();
            paths.push("children");
            let get_children_url = self.base_url.append_paths(&paths);

            // Gather sub-directories to process.
            let mut list_directories_url = get_children_url.clone();
            list_directories_url.set_query(Some("$select=id&$filter=folder ne null&$top=999999"));
            directory_sender
                .send(self.get_all_ids(token, list_directories_url))
                .ok()
                .unwrap();

            // Gather images.
            let mut list_images_url = get_children_url;
            list_images_url.set_query(Some("$select=id&$filter=image ne null&$top=999999"));
            image_sender
                .send(self.get_all_ids(token, list_images_url))
                .ok()
                .unwrap();
        };

        // Seed with initial directories.
        for directory in config.directories {
            process_directory(format!("root:/{directory}:"));
        }

        // Depth-first processing of directories...
        while let Ok(directories) = directory_receiver.try_recv() {
            for directory_item in directories.await.with_context(|| "Get sub-directories")? {
                let id = directory_item.id;
                process_directory(format!("items/{id}"));
            }
        }

        let mut all_images = Vec::new();
        while let Ok(images) = image_receiver.try_recv() {
            all_images.extend(
                images
                    .await
                    .with_context(|| "Get images")?
                    .into_iter()
                    .map(|DriveItem { id }| id),
            )
        }

        Ok((all_images, config.interval))
    }

    pub async fn load_next(
        &self,
        token: &str,
        height: u32,
        width: u32,
        all_images: &[String],
    ) -> Result<ColorImage> {
        let index = rand::thread_rng().gen_range(0..all_images.len());
        let image_id = all_images.get(index).unwrap();

        let cache_path = self.cache_directory.join(image_id);
        let data = if cache_path.exists() {
            tokio::fs::read(cache_path)
                .await
                .with_context(|| "Reading cached image failed")?
                .into()
        } else {
            let mut thumbnail_url = self
                .base_url
                .append_paths(&["items", image_id, "thumbnails"]);
            thumbnail_url.set_query(Some(&format!("select=c{height}x{width}")));
            let thumbnail_response = self
                .client
                .get::<ThumbnailResponse>(token, thumbnail_url)
                .await
                .with_context(|| "Get thumbnail")?;
            let (_, ThumbnailItem { url: download_url }) = thumbnail_response
                .value
                .into_iter()
                .next()
                .ok_or(anyhow!("Bad thumbnail response"))?
                .into_iter()
                .next()
                .ok_or(anyhow!("No thumbnail returned"))?;
            let data = self
                .client
                .download(
                    token,
                    Url::parse(&download_url).with_context(|| "Download URL invalid")?,
                )
                .await
                .with_context(|| "Downloading image failed")?;

            if should_cache_image() {
                if !self.cache_directory.exists() {
                    tokio::fs::create_dir_all(&self.cache_directory)
                        .await
                        .with_context(|| "Create cache directory")?;
                }
                tokio::fs::write(&cache_path, &data)
                    .await
                    .with_context(|| "Store image in cache")?;
            }

            data
        };

        let image = image::load_from_memory(&data)
            .map_err(|err| anyhow!(err).context("Image parsing failed"))?;
        let size = [image.width() as _, image.height() as _];
        let image_buffer = image.to_rgba8();
        let pixels = image_buffer.as_flat_samples();
        Ok(egui::ColorImage::from_rgba_unmultiplied(
            size,
            pixels.as_slice(),
        ))
    }
}

fn should_cache_image() -> bool {
    cfg_if::cfg_if! {
        if #[cfg(test)] {
            true
        } else {
            let disk_info = sys_info::disk_info().unwrap();
            disk_info.free >= disk_info.total / 10
        }
    }
}

#[tokio::test]
async fn list_images() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let config_content_redirect_mock = server
        .mock("GET", "/root:/slideshow.txt:/content")
        .match_header("authorization", "Bearer token")
        .with_status(302)
        .with_header("location", &format!("{url}/slideshow.txt"))
        .expect(1)
        .create();

    let config_content_mock = server
        .mock("GET", "/slideshow.txt")
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "directories": [ "d1", "d2" ], "interval": 42 } "#)
        .expect(1)
        .create();

    let folder_query = mockito::Matcher::AllOf(vec![
        mockito::Matcher::UrlEncoded("$select".into(), "id".into()),
        mockito::Matcher::UrlEncoded("$filter".into(), "folder ne null".into()),
    ]);
    let image_query = mockito::Matcher::AllOf(vec![
        mockito::Matcher::UrlEncoded("$select".into(), "id".into()),
        mockito::Matcher::UrlEncoded("$filter".into(), "image ne null".into()),
    ]);

    let d1_folder_mock = server
        .mock("GET", "/root:/d1:/children")
        .match_query(folder_query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(format!(
            r#"{{ "@odata.nextLink": "{url}/d1_folder_next", "value": [ {{ "id": "d1_1" }} ] }}"#
        ))
        .expect(1)
        .create();
    let d1_folder_next_mock = server
        .mock("GET", "/d1_folder_next")
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ { "id": "d1_2" } ] }"#)
        .expect(1)
        .create();
    let d1_images_mock = server
        .mock("GET", "/root:/d1:/children")
        .match_query(image_query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(format!(
            r#"{{ "@odata.nextLink": "{url}/d1_image_next", "value": [ {{ "id": "d1_3" }} ] }}"#
        ))
        .expect(1)
        .create();
    let d1_image_next_mock = server
        .mock("GET", "/d1_image_next")
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ { "id": "d1_4" } ] }"#)
        .expect(1)
        .create();

    let d2_folder_mock = server
        .mock("GET", "/root:/d2:/children")
        .match_query(folder_query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ ] }"#)
        .expect(1)
        .create();
    let d2_image_mock = server
        .mock("GET", "/root:/d2:/children")
        .match_query(image_query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ { "id": "d2_1" } ] }"#)
        .expect(1)
        .create();

    let d1_1_folder_mock = server
        .mock("GET", "/items/d1_1/children")
        .match_query(folder_query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ ] }"#)
        .expect(1)
        .create();
    let d1_1_image_mock = server
        .mock("GET", "/items/d1_1/children")
        .match_query(image_query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ { "id": "d1_1_1" } ] }"#)
        .expect(1)
        .create();

    let d1_2_folder_mock = server
        .mock("GET", "/items/d1_2/children")
        .match_query(folder_query)
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ ] }"#)
        .expect(1)
        .create();
    let d1_2_image_next_mock = server
        .mock("GET", "/items/d1_2/children")
        .match_query(image_query)
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ { "id": "d1_2_1" } ] }"#)
        .expect(1)
        .create();

    let temp_dir = std::env::temp_dir().join("onedrive_slideshow_test/list_images");
    let image_loader = ImageLoader::new(&url, temp_dir);
    let (mut all_images, interval) = image_loader.get_image_list("token").await.unwrap();
    all_images.sort();
    assert_eq!(interval, 42);
    assert_eq!(&all_images, &["d1_1_1", "d1_2_1", "d1_3", "d1_4", "d2_1"]);

    config_content_redirect_mock.assert();
    config_content_mock.assert();
    d1_folder_mock.assert();
    d1_folder_next_mock.assert();
    d1_images_mock.assert();
    d1_image_next_mock.assert();
    d2_folder_mock.assert();
    d2_image_mock.assert();
    d1_1_folder_mock.assert();
    d1_1_image_mock.assert();
    d1_2_folder_mock.assert();
    d1_2_image_next_mock.assert();
}

#[tokio::test]
async fn load_image() {
    let temp_dir = std::env::temp_dir().join("onedrive_slideshow_test/load_image");
    if temp_dir.exists() {
        tokio::fs::remove_dir_all(&temp_dir).await.unwrap();
    }

    let mut server = mockito::Server::new();
    let url = server.url();

    let thumbnail_mock = server
        .mock("GET", "/items/1/thumbnails")
        .match_query(mockito::Matcher::UrlEncoded(
            "select".into(),
            "c1024x768".into(),
        ))
        .match_header("authorization", "Bearer token")
        .with_body(format!(
            r#"{{ "value": [ {{ "c1024x768": {{ "url": "{url}/download" }} }} ] }} "#
        ))
        .expect(1)
        .create();

    let mut image_data = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut image_data)
        .encode_image(&image::RgbImage::new(1, 1))
        .unwrap();
    let download_mock = server
        .mock("GET", "/download")
        .with_body(image_data)
        .expect(1)
        .create();

    let image_loader = ImageLoader::new(&url, temp_dir);
    let actual_image = image_loader
        .load_next("token", 1024, 768, &["1".into()])
        .await
        .unwrap();
    assert_eq!(actual_image.height(), 1);
    assert_eq!(actual_image.width(), 1);
    thumbnail_mock.assert();
    download_mock.assert();

    // Loading again should use the cached image.
    thumbnail_mock.remove();
    download_mock.remove();
    let actual_image = image_loader
        .load_next("token", 1024, 768, &["1".into()])
        .await
        .unwrap();
    assert_eq!(actual_image.height(), 1);
    assert_eq!(actual_image.width(), 1);

    // But loading a different image will download again.
    let thumbnail_mock = server
        .mock("GET", "/items/2/thumbnails")
        .match_query(mockito::Matcher::UrlEncoded(
            "select".into(),
            "c1024x768".into(),
        ))
        .match_header("authorization", "Bearer token")
        .with_body(format!(
            r#"{{ "value": [ {{ "c1024x768": {{ "url": "{url}/download" }} }} ] }} "#
        ))
        .expect(1)
        .create();

    let mut image_data = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut image_data)
        .encode_image(&image::RgbImage::new(2, 2))
        .unwrap();
    let download_mock = download_mock.with_body(image_data).create();
    let actual_image = image_loader
        .load_next("token", 1024, 768, &["2".into()])
        .await
        .unwrap();
    assert_eq!(actual_image.height(), 2);
    assert_eq!(actual_image.width(), 2);
    thumbnail_mock.assert();
    download_mock.assert();
}
