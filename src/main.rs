#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

mod auth;
mod http;
mod image_loader;

use anyhow::Result;
use auth::Authenticator;
use eframe::{
    egui::{self, RichText},
    epaint::{Color32, Rect},
};
use egui_extras::RetainedImage;
use image_loader::ImageLoader;
use std::{process, time::Duration};
use tokio::{
    sync::mpsc::{channel, error::TryRecvError, Receiver, Sender},
    task,
    time::Instant,
};

use crate::auth::AuthMessage;

const ON_ERROR_REFRESH_TIME: Duration = Duration::from_secs(1);
const IMAGE_LIST_REFRESH_TIME: Duration = Duration::from_secs(60 * 60);

fn main() -> Result<(), eframe::Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let options = eframe::NativeOptions {
                fullscreen: true,
                ..Default::default()
            };
            eframe::run_native(
                "OneDrive Slideshow",
                options,
                Box::new(move |cc| {
                    let (sender, receiver) = channel(8);
                    task::spawn(image_load_loop(sender, cc.egui_ctx.clone()));
                    Box::new(Slideshow::new(receiver))
                }),
            )
        })
}

enum AppState {
    WaitingForAuth(String, String),
    LoadingImage,
    HasImage(RetainedImage),
}

unsafe impl Send for AppState {}
unsafe impl Sync for AppState {}

struct Slideshow {
    current_state: Result<AppState>,
    incoming_state: Receiver<Result<AppState>>,
}

impl Slideshow {
    fn new(image_receiver: Receiver<Result<AppState>>) -> Self {
        Self {
            current_state: Ok(AppState::LoadingImage),
            incoming_state: image_receiver,
        }
    }
}

impl eframe::App for Slideshow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // If it's been long enough between updates, then start getting another image and switch images.
        match self.incoming_state.try_recv() {
            Ok(new_state) => self.current_state = new_state,
            Err(TryRecvError::Disconnected) => process::exit(1),
            _ => (),
        }

        egui::CentralPanel::default().show(ctx, |ui|
            ui.centered_and_justified(|ui|
                 match &self.current_state {
                    Ok(AppState::LoadingImage) => {
                        ui.spinner();
                    }
                    Ok(AppState::HasImage(image)) => {
                        let image_size = image.size_vec2();
                        let screen_size = ui.available_size();
                        let x_ratio = image_size.x / screen_size.x;
                        let y_ratio = image_size.y / screen_size.y;

                        ctx.set_cursor_icon(egui::CursorIcon::None);
                        image.show_size(ui, image_size / x_ratio.max(y_ratio));
                    }
                    Ok(AppState::WaitingForAuth(auth_url, code)) => {
                        ui.label(RichText::new(format!("Authorize the slideshow to read from your OneDrive by opening {auth_url} in a browser and entering the code {code}")).size(20.0).color(Color32::WHITE));
                    }
                    Err(err) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("{err:?}")); // something went wrong
                    }
                }));
    }
}

struct ImageList {
    images: Vec<String>,
    interval: Duration,
    refresh_after: Instant,
}

async fn image_load_loop(ui_sender: Sender<Result<AppState>>, ctx: egui::Context) {
    let (auth_sender, mut auth_receiver) = channel(8);
    let captured_ui_sender = ui_sender.clone();
    let captured_ctx = ctx.clone();
    let _auth_manager = task::spawn(async move {
        while let Some(message) = auth_receiver.recv().await {
            match message {
                AuthMessage::HasClientCode(auth_url, code) => {
                    send_update(
                        &captured_ui_sender,
                        &captured_ctx,
                        Ok(AppState::WaitingForAuth(auth_url, code)),
                    )
                    .await;
                }
                AuthMessage::Completed => {
                    send_update(
                        &captured_ui_sender,
                        &captured_ctx,
                        Ok(AppState::LoadingImage),
                    )
                    .await;
                }
            }
        }
    });

    let mut authenticator = Authenticator::new(
        auth_sender,
        "https://login.microsoftonline.com/consumers/oauth2/v2.0",
    );
    let loader = ImageLoader::new(
        "https://graph.microsoft.com/v1.0/me/drive",
        std::env::temp_dir().join("onedrive_slideshow"),
    );
    let mut next_image = get_next_image(
        &loader,
        get_auth_token(&mut authenticator, &ui_sender, &ctx).await,
        ctx.screen_rect(),
        None,
    );
    let mut interval = Duration::ZERO;
    loop {
        tokio::time::sleep(interval).await;

        let all_images = match next_image.await {
            Ok((image, all_images)) => {
                interval = all_images.interval;
                send_update(&ui_sender, &ctx, Ok(AppState::HasImage(image))).await;
                Some(all_images)
            }
            Err((err, all_images)) => {
                interval = ON_ERROR_REFRESH_TIME;
                send_update(&ui_sender, &ctx, Err(err.context("Loading image"))).await;
                all_images
            }
        };

        next_image = get_next_image(
            &loader,
            get_auth_token(&mut authenticator, &ui_sender, &ctx).await,
            ctx.screen_rect(),
            all_images,
        );
    }
}

async fn get_auth_token(
    authenticator: &mut Authenticator,
    ui_sender: &Sender<Result<AppState>>,
    ctx: &egui::Context,
) -> String {
    loop {
        match authenticator.get_token().await {
            Ok(token) => return token,
            Err(err) => {
                send_update(ui_sender, ctx, Err(err.context("Authenticating"))).await;
            }
        }
    }
}

async fn get_next_image(
    loader: &ImageLoader,
    token: String,
    size: Rect,
    mut all_images: Option<ImageList>,
) -> Result<(RetainedImage, ImageList), (anyhow::Error, Option<ImageList>)> {
    // Check for expiry.
    if all_images
        .as_ref()
        .map_or(false, |list| Instant::now() >= list.refresh_after)
    {
        all_images = None;
    }

    // Get the new list of images if we don't have one.
    let all_images = if let Some(all_images) = all_images {
        all_images
    } else {
        let (images, interval) = loader
            .get_image_list(&token)
            .await
            .map_err(|err| (err, None))?;
        ImageList {
            images,
            interval: Duration::from_secs(interval),
            refresh_after: Instant::now().checked_add(IMAGE_LIST_REFRESH_TIME).unwrap(),
        }
    };

    match loader
        .load_next(
            &token,
            size.height() as u32,
            size.width() as u32,
            &all_images.images,
        )
        .await
    {
        Ok(image) => Ok((image, all_images)),
        Err(err) => Err((err, Some(all_images))),
    }
}

async fn send_update<T>(sender: &Sender<T>, ctx: &egui::Context, message: T) {
    if sender.send(message).await.is_err() {
        process::exit(1);
    }
    ctx.request_repaint();
}

#[tokio::test]
async fn load_multiple_images() {
    let temp_dir = std::env::temp_dir().join("onedrive_slideshow_test/load_multiple_images");
    if temp_dir.exists() {
        tokio::fs::remove_dir_all(&temp_dir).await.unwrap();
    }

    let mut server = mockito::Server::new();
    let url = server.url();

    let config_content_mock = server
        .mock("GET", "/root:/slideshow.txt:/content")
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "directories": [ "d1" ], "interval": 42 } "#)
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
        .with_body(r#"{ "value": [ ] }"#)
        .expect(1)
        .create();
    let d1_image_mock = server
        .mock("GET", "/root:/d1:/children")
        .match_query(image_query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ { "id": "the_image" } ] }"#)
        .expect(1)
        .create();

    let thumbnail_mock = server
        .mock("GET", "/items/the_image/thumbnails")
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

    // First load should get the config and directory listing.
    let image_loader = ImageLoader::new(&url, temp_dir);
    let (actual_image, all_images) = get_next_image(
        &image_loader,
        "token".into(),
        Rect {
            min: eframe::epaint::Pos2::ZERO,
            max: eframe::epaint::Pos2 {
                y: 1024.0,
                x: 768.0,
            },
        },
        None,
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(actual_image.height(), 1);
    assert_eq!(actual_image.width(), 1);
    assert_eq!(all_images.images, &["the_image".to_string()]);
    config_content_mock.assert();
    d1_folder_mock.assert();
    d1_image_mock.assert();
    thumbnail_mock.assert();
    download_mock.assert();

    // Second load should be entirely offline since it will use the cache.
    config_content_mock.remove();
    d1_folder_mock.remove();
    d1_image_mock.remove();
    thumbnail_mock.remove();
    download_mock.remove();
    let (actual_image, mut all_images) = get_next_image(
        &image_loader,
        "token".into(),
        Rect {
            min: eframe::epaint::Pos2::ZERO,
            max: eframe::epaint::Pos2 {
                y: 1024.0,
                x: 768.0,
            },
        },
        Some(all_images),
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(actual_image.height(), 1);
    assert_eq!(actual_image.width(), 1);
    assert_eq!(all_images.images, &["the_image".to_string()]);

    // Make the image list expire: this will cause it to reload, but the image should come from cache.
    let config_content_mock = config_content_mock.create();
    let d1_folder_mock = d1_folder_mock.create();
    let d1_image_mock = d1_image_mock.create();
    all_images.refresh_after = Instant::now();
    let (actual_image, all_images) = get_next_image(
        &image_loader,
        "token".into(),
        Rect {
            min: eframe::epaint::Pos2::ZERO,
            max: eframe::epaint::Pos2 {
                y: 1024.0,
                x: 768.0,
            },
        },
        Some(all_images),
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(actual_image.height(), 1);
    assert_eq!(actual_image.width(), 1);
    assert_eq!(all_images.images, &["the_image".to_string()]);
    config_content_mock.assert();
    d1_folder_mock.assert();
    d1_image_mock.assert();
}
