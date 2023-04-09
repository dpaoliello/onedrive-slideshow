#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

mod auth;
mod http;
mod image_loader;

use anyhow::Result;
use auth::Authenticator;
use eframe::{egui, epaint::Rect};
use egui_extras::RetainedImage;
use image_loader::ImageLoader;
use std::{process, time::Duration};
use tokio::{
    sync::mpsc::{channel, error::TryRecvError, Receiver, Sender},
    task,
    time::Instant,
};

const DEFAULT_IMAGE_REFRESH_TIME: Duration = Duration::from_secs(1);
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

                        image.show_size(ui, image_size / x_ratio.max(y_ratio));
                    }
                    Ok(AppState::WaitingForAuth(auth_url, code)) => {
                        ui.label(format!("Authorize the slideshow to read from your OneDrive by opening {auth_url} in a browser and entering the code {code}"));
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
    last_updated: Instant,
}

async fn image_load_loop(ui_sender: Sender<Result<AppState>>, ctx: egui::Context) {
    let (auth_sender, mut auth_receiver) = channel(8);
    let captured_ui_sender = ui_sender.clone();
    let captured_ctx = ctx.clone();
    let _auth_manager = task::spawn(async move {
        while let Some((auth_url, code)) = auth_receiver.recv().await {
            send_update(
                &captured_ui_sender,
                &captured_ctx,
                Ok(AppState::WaitingForAuth(auth_url, code)),
            )
            .await;
        }
    });

    let mut loader = ImageLoader::new(Authenticator::new(auth_sender));
    let mut all_images = None;
    loop {
        match get_next_image(&mut loader, ctx.screen_rect(), &mut all_images).await {
            Ok(image) => send_update(&ui_sender, &ctx, Ok(AppState::HasImage(image))).await,
            Err(err) => {
                send_update(&ui_sender, &ctx, Err(err.context("Loading image"))).await;
            }
        }

        tokio::time::sleep(
            all_images
                .as_ref()
                .map_or(DEFAULT_IMAGE_REFRESH_TIME, |all_images| all_images.interval),
        )
        .await;
    }
}

async fn get_next_image(
    loader: &mut ImageLoader,
    size: Rect,
    all_images: &mut Option<ImageList>,
) -> Result<RetainedImage> {
    // Check for expiry.
    if all_images.as_ref().map_or(false, |list| {
        Instant::now().duration_since(list.last_updated) >= IMAGE_LIST_REFRESH_TIME
    }) {
        *all_images = None;
    }

    // Get the new list of images if we don't have one.
    if all_images.is_none() {
        let (images, interval) = loader.get_image_list().await?;
        *all_images = Some(ImageList {
            images,
            interval: Duration::from_secs(interval),
            last_updated: Instant::now(),
        });
    }

    loader
        .load_next(
            size.height() as u32,
            size.width() as u32,
            &all_images.as_ref().unwrap().images,
        )
        .await
}

async fn send_update<T>(sender: &Sender<T>, ctx: &egui::Context, message: T) {
    if sender.send(message).await.is_err() {
        process::exit(1);
    }
    ctx.request_repaint();
}
