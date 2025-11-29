use crate::http::{AppendPaths, Client};
use anyhow::{bail, Context, Result};
use rand::Rng;
use reqwest::Url;
use serde::Deserialize;
use sysinfo::Disks;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) struct ItemLoader {
    client: Client,
    base_url: Url,
    config_url: Url,
    cache_directory: PathBuf,
}

#[cfg_attr(test, derive(Eq, PartialEq, Debug, PartialOrd, Ord))]
#[derive(Clone)]
pub(crate) enum Item {
    Image(String),
    Video(String, Duration),
}

impl Item {
    pub fn get_id(&self) -> &str {
        match self {
            Item::Image(id) | Item::Video(id, ..) => id,
        }
    }
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
struct DriveVideo {
    duration: u64,
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
    video: Option<DriveVideo>,
}

#[derive(Deserialize)]
struct Config {
    directories: Vec<String>,
    interval: u64,
}

impl ItemLoader {
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

    pub async fn get_item_list(&self, token: &str) -> Result<(Vec<Item>, u64)> {
        let config = self
            .client
            .get::<Config>(token, self.config_url.clone())
            .await
            .with_context(|| "Get slideshow.txt")?;

        let process_directory = |directory: String| {
            let mut paths = directory.split('/').collect::<Vec<_>>();
            paths.push("children");
            let mut get_children_url = self.base_url.append_paths(&paths);
            get_children_url.set_query(Some("select=id,image,folder,video&top=1000"));

            self.get_all_items(token, get_children_url)
        };

        // Seed with initial directories.
        let mut directories_to_process = Vec::new();
        for directory in config.directories {
            directories_to_process.push(process_directory(format!("root:/{directory}:")));
        }

        let mut all_items = Vec::new();
        while let Some(items) = directories_to_process.pop() {
            let items = items.await.with_context(|| "Getting items")?;
            // Assume that most items are items to display.
            all_items.reserve(items.len());
            for item in items {
                match item {
                    DriveItem {
                        id, image: Some(_), ..
                    } => all_items.push(Item::Image(id)),
                    DriveItem {
                        id,
                        video: Some(DriveVideo { duration }),
                        ..
                    } => all_items.push(Item::Video(id, Duration::from_millis(duration))),
                    DriveItem {
                        id,
                        folder: Some(_),
                        ..
                    } => directories_to_process.push(process_directory(format!("items/{id}"))),
                    _ => {}
                }
            }
        }

        Ok((all_items, config.interval))
    }

    pub async fn load_next(&self, token: &str, all_items: &[Item]) -> Result<Item> {
        let index = rand::rng().random_range(0..all_items.len());
        let item = all_items.get(index).unwrap();
        let id = item.get_id();

        let cache_path = self.cache_directory.join(id);
        if !cache_path.exists() {
            let content_url = self.base_url.append_paths(&["items", id, "content"]);
            let data = self
                .client
                .download(token, content_url)
                .await
                .with_context(|| "Downloading item failed")?;

            self.prepare_cache().await?;

            tokio::fs::write(&cache_path, &data)
                .await
                .with_context(|| "Store item in cache")?;
        }

        Ok(item.clone())
    }

    async fn prepare_cache(&self) -> Result<()> {
        if !self.cache_directory.exists() {
            tokio::fs::create_dir_all(&self.cache_directory)
                .await
                .with_context(|| "Create cache directory")?;
        }

        loop {
            if get_free_space_percent_for_path(&self.cache_directory)? >= 10.0 {
                return Ok(());
            }

            let mut dir_listing = tokio::fs::read_dir(&self.cache_directory)
                .await
                .with_context(|| "Get cache directory listing for cleaning")?;

            let first_file = loop {
                let Some(entry) = dir_listing
                    .next_entry()
                    .await
                    .with_context(|| "Get file to clean")?
                else {
                    bail!("Not enough disk space, but no files in cache to delete");
                };

                if entry
                    .metadata()
                    .await
                    .with_context(|| "Get metadata of file to clean")?
                    .is_file()
                {
                    break entry;
                }
            };

            tokio::fs::remove_file(first_file.path())
                .await
                .with_context(|| "Delete file in cache to make space")?;
        }
    }
}

fn get_free_space_percent_for_path(path: &Path) -> Result<f32> {
    let resolved_path = fs::canonicalize(path)?;

    for disk in &Disks::new_with_refreshed_list() {
        if resolved_path.starts_with(fs::canonicalize(disk.mount_point())?) {
            return Ok(disk.available_space() as f32 / disk.total_space() as f32 * 100.0);
        }
    }

    Err(anyhow::anyhow!("No matching disk found"))
}

#[tokio::test(flavor = "multi_thread")]
async fn list_items() {
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

    let query = mockito::Matcher::UrlEncoded("select".into(), "id,image,folder,video".into());

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
                { "id": "d1_4", "video" : { "duration": 1024 } }
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
            "value": [ { "id": "d1_2_1", "video": { "duration": 100 } } ]
        }"#,
        )
        .expect(1)
        .create();

    let temp_dir = std::env::temp_dir().join("onedrive_slideshow_test/list_items");
    let item_loader = ItemLoader::new(&url, temp_dir);
    let (mut all_items, interval) = item_loader.get_item_list("token").await.unwrap();
    all_items.sort();
    assert_eq!(interval, 42);
    assert_eq!(
        &all_items,
        &[
            Item::Image("d1_1_1".to_string()),
            Item::Image("d1_3".to_string()),
            Item::Image("d2_1".to_string()),
            Item::Video("d1_2_1".to_string(), Duration::from_millis(100)),
            Item::Video("d1_4".to_string(), Duration::from_millis(1024)),
        ]
    );

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

    let item_loader = ItemLoader::new(&url, temp_dir.clone());
    let test_item = Item::Image("1".to_string());
    let actual_image = item_loader
        .load_next("token", std::slice::from_ref(&test_item))
        .await
        .unwrap();
    assert_eq!(actual_image, test_item);
    assert_eq!(
        temp_dir.clone().join("1").to_str().unwrap(),
        temp_dir
            .join(test_item.get_id())
            .to_string_lossy()
            .into_owned()
    );
    content_mock.assert();

    // Loading again should use the cached image.
    content_mock.remove();
    let actual_image = item_loader
        .load_next("token", std::slice::from_ref(&test_item))
        .await
        .unwrap();
    assert_eq!(actual_image, test_item);

    // But loading a different image will download again.
    let content_mock = server
        .mock("GET", "/items/2/content")
        .match_header("authorization", "Bearer token")
        .with_body(b"0")
        .expect(1)
        .create();

    let test_item = Item::Image("2".to_string());
    let actual_image = item_loader
        .load_next("token", std::slice::from_ref(&test_item))
        .await
        .unwrap();
    assert_eq!(actual_image, test_item);
    assert_eq!(
        temp_dir.clone().join("2").to_str().unwrap(),
        temp_dir
            .join(test_item.get_id())
            .to_string_lossy()
            .into_owned()
    );
    content_mock.assert();
}
