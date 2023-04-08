use crate::{auth::Authenticator, http::Client};
use anyhow::{anyhow, Context, Result};
use egui_extras::RetainedImage;
use rand::Rng;
use serde::Deserialize;
use std::collections::HashMap;

pub struct ImageLoader {
    client: Client,
    authenticator: Authenticator,
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
    pub fn new(authenticator: Authenticator) -> Self {
        Self {
            client: Client::new(),
            authenticator,
        }
    }

    async fn get_all_ids(&mut self, first_url: &str) -> Result<Vec<DriveItem>> {
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
                .get::<DriveResponse>(&mut self.authenticator, &url)
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
            .get::<Config>(
                &mut self.authenticator,
                "https://graph.microsoft.com/v1.0/me/drive/root:/slideshow.txt:/content",
            )
            .await
            .with_context(|| "Get slideshow.txt")?;

        let mut all_images = Vec::new();
        let mut directory_work_list = config
            .directories
            .iter()
            .map(|d| format!("root:/{d}:"))
            .collect::<Vec<_>>();
        while let Some(directory) = directory_work_list.pop() {
            // Gather sub-directories to process.
            let list_directories_url = format!("https://graph.microsoft.com/v1.0/me/drive/{directory}/children?$select=id&$filter=folder ne null&$top=999999");
            directory_work_list.extend(
                self.get_all_ids(&list_directories_url)
                    .await
                    .with_context(|| "Get sub-directories")?
                    .into_iter()
                    .map(|DriveItem { id }| format!("items/{id}")),
            );

            // Gather images.
            let list_images_url = format!("https://graph.microsoft.com/v1.0/me/drive/{directory}/children?$select=id&$filter=image ne null&$top=999999");
            all_images.extend(
                self.get_all_ids(&list_images_url)
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

        let thumbnail_url = format!("https://graph.microsoft.com/v1.0/me/drive/items/{image_id}/thumbnails?select=c{height}x{width}");
        let thumbnail_response = self
            .client
            .get::<ThumbnailResponse>(&mut self.authenticator, &thumbnail_url)
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
                .download(&mut self.authenticator, &download_url)
                .await
                .with_context(|| "Downloading image failed")?,
        )
        .map_err(|err| anyhow!(err).context("Image parsing failed"))
    }
}
