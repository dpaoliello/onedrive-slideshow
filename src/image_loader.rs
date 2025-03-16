use crate::http::{AppendPaths, Client};
use anyhow::{Context, Result};
use rand::Rng;
use reqwest::Url;
use serde::Deserialize;
use std::path::PathBuf;

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
struct DriveImage {
    #[expect(dead_code)]
    height: Option<u32>,
    #[expect(dead_code)]
    width: Option<u32>,
}

#[derive(Deserialize)]
struct DriveFolder {
    #[expect(dead_code)]
    #[serde(rename = "childCount")]
    child_count: u32,
}

#[derive(Deserialize)]
struct DriveItem {
    id: String,
    image: Option<DriveImage>,
    folder: Option<DriveFolder>,
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

    async fn get_all_items(&self, token: &str, first_url: Url) -> Result<Vec<DriveItem>> {
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

        let process_directory = |directory: String| {
            let mut paths = directory.split('/').collect::<Vec<_>>();
            paths.push("children");
            let mut get_children_url = self.base_url.append_paths(&paths);
            get_children_url.set_query(Some("select=id,image,folder&top=1000"));

            self.get_all_items(token, get_children_url)
        };

        // Seed with initial directories.
        let mut directories_to_process = Vec::new();
        for directory in config.directories {
            directories_to_process.push(process_directory(format!("root:/{directory}:")));
        }

        let mut all_images = Vec::new();
        while let Some(items) = directories_to_process.pop() {
            let items = items.await.with_context(|| "Getting items")?;
            // Assume that most items are images.
            all_images.reserve(items.len());
            for item in items {
                match item {
                    DriveItem {
                        id, image: Some(_), ..
                    } => all_images.push(id),
                    DriveItem {
                        id,
                        folder: Some(_),
                        ..
                    } => directories_to_process.push(process_directory(format!("items/{id}"))),
                    _ => {}
                }
            }
        }

        Ok((all_images, config.interval))
    }

    pub async fn load_next(&self, token: &str, all_images: &[String]) -> Result<String> {
        let index = rand::rng().random_range(0..all_images.len());
        let image_id = all_images.get(index).unwrap();

        let cache_path = self.cache_directory.join(image_id);
        if !cache_path.exists() {
            let content_url = self.base_url.append_paths(&["items", image_id, "content"]);
            let data = self
                .client
                .download(token, content_url)
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
        }

        Ok(cache_path.to_string_lossy().into_owned())
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

#[tokio::test(flavor = "multi_thread")]
async fn list_images() {
    let mut server = mockito::Server::new_async().await;
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

    let query = mockito::Matcher::UrlEncoded("select".into(), "id,image,folder".into());

    let d1_mock = server
        .mock("GET", "/root:/d1:/children")
        .match_query(query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(format!(
            r#"{{
            "@odata.nextLink": "{url}/d1_next",
            "value": [
                {{ "id": "d1_1", "folder": {{ "childCount": 1 }} }},
                {{ "id": "d1_3", "image": {{ "height": 1024, "width": 768 }} }},
                {{ "id": "d1_ignore" }}
            ] }}"#
        ))
        .expect(1)
        .create();
    let d1_next_mock = server
        .mock("GET", "/d1_next")
        .match_header("authorization", "Bearer token")
        .with_body(
            r#"{
            "value": [
                { "id": "d1_2", "folder": { "childCount": 1 } },
                { "id": "d1_4", "image" : {} }
            ] }"#,
        )
        .expect(1)
        .create();

    let d2_mock = server
        .mock("GET", "/root:/d2:/children")
        .match_query(query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(
            r#"{
            "value": [ { "id": "d2_1", "image": {} } ]
        }"#,
        )
        .expect(1)
        .create();

    let d1_1_mock = server
        .mock("GET", "/items/d1_1/children")
        .match_query(query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(
            r#"{
            "value": [ { "id": "d1_1_1", "image": {} } ]
        }"#,
        )
        .expect(1)
        .create();

    let d1_2_mock = server
        .mock("GET", "/items/d1_2/children")
        .match_query(query)
        .match_header("authorization", "Bearer token")
        .with_body(
            r#"{
            "value": [ { "id": "d1_2_1", "image": {} } ]
        }"#,
        )
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
    d1_mock.assert();
    d1_next_mock.assert();
    d2_mock.assert();
    d1_1_mock.assert();
    d1_2_mock.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn load_image() {
    let temp_dir = std::env::temp_dir().join("onedrive_slideshow_test/load_image");
    if temp_dir.exists() {
        tokio::fs::remove_dir_all(&temp_dir).await.unwrap();
    }

    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let content_mock = server
        .mock("GET", "/items/1/content")
        .match_header("authorization", "Bearer token")
        .with_body(b"0")
        .expect(1)
        .create();

    let image_loader = ImageLoader::new(&url, temp_dir.clone());
    let actual_image = image_loader
        .load_next("token", &["1".into()])
        .await
        .unwrap();
    assert_eq!(actual_image, temp_dir.clone().join("1").to_str().unwrap());
    content_mock.assert();

    // Loading again should use the cached image.
    content_mock.remove();
    let actual_image = image_loader
        .load_next("token", &["1".into()])
        .await
        .unwrap();
    assert_eq!(actual_image, temp_dir.clone().join("1").to_str().unwrap());

    // But loading a different image will download again.
    let content_mock = server
        .mock("GET", "/items/2/content")
        .match_header("authorization", "Bearer token")
        .with_body(b"0")
        .expect(1)
        .create();

    let actual_image = image_loader
        .load_next("token", &["2".into()])
        .await
        .unwrap();
    assert_eq!(actual_image, temp_dir.clone().join("2").to_str().unwrap());
    content_mock.assert();
}
