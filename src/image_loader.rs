use crate::{
    auth::Authenticator,
    http::{AppendPaths, Client},
};
use anyhow::{anyhow, Context, Result};
use egui_extras::RetainedImage;
use rand::Rng;
use reqwest::Url;
use serde::Deserialize;
use std::collections::HashMap;

pub struct ImageLoader {
    client: Client,
    authenticator: Authenticator,
    base_url: Url,
    config_url: Url,
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
    pub fn new(authenticator: Authenticator, base_url: &str) -> Self {
        let base_url = Url::parse(base_url).unwrap();
        Self {
            client: Client::new(),
            authenticator,
            config_url: base_url.append_paths(&["root:", "slideshow.txt:", "content"]),
            base_url,
        }
    }

    async fn get_all_ids(&mut self, first_url: Url) -> Result<Vec<DriveItem>> {
        let response = self
            .client
            .get::<DriveResponse>(&mut self.authenticator, first_url)
            .await
            .with_context(|| "Get all items")?;
        let mut items = response.value;
        let mut next_url = response.next_link;
        while let Some(url) = next_url {
            let response = self
                .client
                .get::<DriveResponse>(
                    &mut self.authenticator,
                    Url::parse(&url).with_context(|| "Next link invalid")?,
                )
                .await
                .with_context(|| "Get all items - next link")?;
            next_url = response.next_link;
            items.extend(response.value);
        }
        Ok(items)
    }

    pub async fn get_image_list(&mut self) -> Result<(Vec<String>, u64)> {
        let config = self
            .client
            .get::<Config>(&mut self.authenticator, self.config_url.clone())
            .await
            .with_context(|| "Get slideshow.txt")?;

        let mut all_images = Vec::new();
        let mut directory_work_list = config
            .directories
            .iter()
            .map(|d| format!("root:/{d}:"))
            .collect::<Vec<_>>();
        while let Some(directory) = directory_work_list.pop() {
            let mut paths = directory.split('/').collect::<Vec<_>>();
            paths.push("children");
            let get_children_url = self.base_url.append_paths(&paths);

            // Gather sub-directories to process.
            let mut list_directories_url = get_children_url.clone();
            list_directories_url.set_query(Some("$select=id&$filter=folder ne null&$top=999999"));
            directory_work_list.extend(
                self.get_all_ids(list_directories_url)
                    .await
                    .with_context(|| "Get sub-directories")?
                    .into_iter()
                    .map(|DriveItem { id }| format!("items/{id}")),
            );

            // Gather images.
            let mut list_images_url = get_children_url;
            list_images_url.set_query(Some("$select=id&$filter=image ne null&$top=999999"));
            all_images.extend(
                self.get_all_ids(list_images_url)
                    .await
                    .with_context(|| "Get images")?
                    .into_iter()
                    .map(|DriveItem { id }| id),
            );
        }

        Ok((all_images, config.interval))
    }

    pub async fn load_next(
        &mut self,
        height: u32,
        width: u32,
        all_images: &[String],
    ) -> Result<RetainedImage> {
        let index = rand::thread_rng().gen_range(0..all_images.len());
        let image_id = all_images.get(index).unwrap();

        let mut thumbnail_url = self
            .base_url
            .append_paths(&["items", image_id, "thumbnails"]);
        thumbnail_url.set_query(Some(&format!("select=c{height}x{width}")));
        let thumbnail_response = self
            .client
            .get::<ThumbnailResponse>(&mut self.authenticator, thumbnail_url)
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

        RetainedImage::from_image_bytes(
            "downloaded_image",
            &self
                .client
                .download(
                    &mut self.authenticator,
                    Url::parse(&download_url).with_context(|| "Download URL invalid")?,
                )
                .await
                .with_context(|| "Downloading image failed")?,
        )
        .map_err(|err| anyhow!(err).context("Image parsing failed"))
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

    let mut image_loader = ImageLoader::new(crate::auth::test_authenticator(), &url);
    let (mut all_images, interval) = image_loader.get_image_list().await.unwrap();
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

    let mut image_loader = ImageLoader::new(crate::auth::test_authenticator(), &url);
    let actual_image = image_loader
        .load_next(1024, 768, &["1".into()])
        .await
        .unwrap();
    assert_eq!(actual_image.height(), 1);
    assert_eq!(actual_image.width(), 1);
    thumbnail_mock.assert();
    download_mock.assert();
}
