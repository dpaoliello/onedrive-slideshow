#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

mod auth;
mod cred_store;
mod http;
mod image_loader;

use anyhow::Result;
use auth::Authenticator;
use image_loader::ImageLoader;
use std::{borrow::Cow, time::Duration};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::WindowBuilder,
};
use tokio::{sync::mpsc::channel, task, time::Instant};
use wry::{WebViewBuilder, WebViewId};

use crate::auth::AuthMessage;

enum UserEvent {
    PreviousImage,
    Error(anyhow::Error),
    Loading,
    WaitingForAuth { auth_url: String, code: String },
    LoadImage(String),
}

const ON_ERROR_REFRESH_TIME: Duration = Duration::from_secs(1);
const IMAGE_LIST_REFRESH_TIME: Duration = Duration::from_secs(60 * 60);

fn protocol_handler(
    _: WebViewId,
    request: wry::http::Request<Vec<u8>>,
) -> wry::http::Response<Cow<'static, [u8]>> {
    let path = &request.uri().path()[1..];
    let content = Cow::Owned(std::fs::read(path).unwrap());
    wry::http::Response::builder()
        .header(wry::http::header::CONTENT_TYPE, "image/jpeg")
        .header(wry::http::header::CACHE_CONTROL, "no-store")
        .body(content)
        .unwrap()
}

fn main() -> Result<(), wry::Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
            let window = WindowBuilder::new()
                .with_decorations(false)
                .with_fullscreen(Some(tao::window::Fullscreen::Borderless(None)))
                .build(&event_loop)
                .unwrap();

            let proxy = event_loop.create_proxy();
            let handler = move |req: wry::http::Request<String>| {
                if req.body() == "onClick" {
                    let _ = proxy.send_event(UserEvent::PreviousImage);
                }
            };

            let builder = WebViewBuilder::new()
                .with_custom_protocol("slideshow".to_string(), protocol_handler)
                .with_ipc_handler(handler)
                .with_accept_first_mouse(true);

            #[cfg(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "ios",
                target_os = "android"
            ))]
            let webview = builder.build(&window)?;
            #[cfg(not(any(
                target_os = "windows",
                target_os = "macos",
                target_os = "ios",
                target_os = "android"
            )))]
            let webview = {
                use tao::platform::unix::WindowExtUnix;
                use wry::WebViewBuilderExtUnix;
                let vbox = window.default_vbox().unwrap();
                builder.build_gtk(vbox)?
            };

            task::spawn(image_load_loop(event_loop.create_proxy()));
            let mut current_image = None;
            let mut previous_image = None;

            event_loop.run(move |event, _, control_flow| {
                *control_flow = ControlFlow::Wait;

                match event {
                    Event::WindowEvent {
                        event: WindowEvent::CloseRequested,
                        ..
                    } => *control_flow = ControlFlow::Exit,

                    Event::UserEvent(event) => match event {
                        UserEvent::Loading => {
                            webview
                                .load_html(include_str!("../ui/loading.html"))
                                .unwrap();
                        }
                        UserEvent::WaitingForAuth { auth_url, code } => {
                            let html = include_str!("../ui/auth.html")
                                .replace("AUTH_URL", &auth_url)
                                .replace("CODE", &code);
                            webview.load_html(&html).unwrap();
                        }
                        UserEvent::LoadImage(image) => {
                            let html =
                                include_str!("../ui/image.html").replace("IMAGE_SRC", &image);
                            previous_image = current_image.take();
                            current_image = Some(image);
                            webview.load_html(&html).unwrap();
                        }
                        UserEvent::PreviousImage => {
                            if let Some(image) = previous_image.take() {
                                let html =
                                    include_str!("../ui/image.html").replace("IMAGE_SRC", &image);
                                previous_image = current_image.take();
                                current_image = Some(image);
                                webview.load_html(&html).unwrap();
                            }
                        }
                        UserEvent::Error(err) => {
                            let html = include_str!("../ui/error.html")
                                .replace("ERROR", &format!("{err:?}"));
                            webview.load_html(&html).unwrap();
                        }
                    },
                    _ => (),
                }
            });
        })
}

async fn image_load_loop(proxy: EventLoopProxy<UserEvent>) {
    let _ = proxy.send_event(UserEvent::Loading);

    let (auth_sender, mut auth_receiver) = channel(8);
    let cloned_proxy = proxy.clone();
    let _auth_manager = task::spawn(async move {
        while let Some(message) = auth_receiver.recv().await {
            match message {
                AuthMessage::HasClientCode(auth_url, code) => {
                    let _ = cloned_proxy.send_event(UserEvent::WaitingForAuth { auth_url, code });
                }
                AuthMessage::Completed => {
                    let _ = cloned_proxy.send_event(UserEvent::Loading);
                }
            }
        }
    });

    let mut authenticator = Authenticator::new(
        auth_sender,
        "https://login.microsoftonline.com/consumers/oauth2/v2.0",
        cred_store::get_refresh_token(),
    );
    let loader = ImageLoader::new(
        "https://graph.microsoft.com/v1.0/me/drive",
        std::env::temp_dir().join("onedrive_slideshow"),
    );
    let mut next_image = get_next_image(
        &loader,
        get_auth_token(&proxy, &mut authenticator).await,
        None,
    );
    let mut interval = Duration::ZERO;
    loop {
        tokio::time::sleep(interval).await;

        let all_images = match next_image.await {
            Ok((image, all_images)) => {
                interval = all_images.interval;
                let _ = proxy.send_event(UserEvent::LoadImage(image));
                Some(all_images)
            }
            Err((err, all_images)) => {
                interval = ON_ERROR_REFRESH_TIME;
                let _ = proxy.send_event(UserEvent::Error(err));
                all_images
            }
        };

        next_image = get_next_image(
            &loader,
            get_auth_token(&proxy, &mut authenticator).await,
            all_images,
        );
    }
}

async fn get_auth_token(
    proxy: &EventLoopProxy<UserEvent>,
    authenticator: &mut Authenticator,
) -> String {
    loop {
        match authenticator.get_token().await {
            Ok(token) => return token,
            Err(err) => {
                let _ = proxy.send_event(UserEvent::Error(err.context("Authenticating")));
            }
        }
    }
}

struct ImageList {
    images: Vec<String>,
    interval: Duration,
    refresh_after: Instant,
}

async fn get_next_image(
    loader: &ImageLoader,
    token: String,
    mut all_images: Option<ImageList>,
) -> Result<(String, ImageList), (anyhow::Error, Option<ImageList>)> {
    // Check for expiry.
    if all_images
        .as_ref()
        .is_some_and(|list| Instant::now() >= list.refresh_after)
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

    match loader.load_next(&token, &all_images.images).await {
        Ok(image) => Ok((image, all_images)),
        Err(err) => Err((err, Some(all_images))),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn load_multiple_images() {
    let temp_dir = std::env::temp_dir().join("onedrive_slideshow_test/load_multiple_images");
    if temp_dir.exists() {
        tokio::fs::remove_dir_all(&temp_dir).await.unwrap();
    }

    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let config_content_mock = server
        .mock("GET", "/root:/slideshow.txt:/content")
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "directories": [ "d1" ], "interval": 42 } "#)
        .expect(1)
        .create();

    let query = mockito::Matcher::UrlEncoded("select".into(), "id,image,folder".into());

    let d1_mock = server
        .mock("GET", "/root:/d1:/children")
        .match_query(query.clone())
        .match_header("authorization", "Bearer token")
        .with_body(r#"{ "value": [ { "id": "the_image", "image": {} } ] }"#)
        .expect(1)
        .create();

    let content_mock = server
        .mock("GET", "/items/the_image/content")
        .match_header("authorization", "Bearer token")
        .with_body(b"0")
        .expect(1)
        .create();

    // First load should get the config and directory listing.
    let image_loader = ImageLoader::new(&url, temp_dir.clone());
    let (actual_image, all_images) = get_next_image(&image_loader, "token".into(), None)
        .await
        .ok()
        .unwrap();
    assert_eq!(
        actual_image,
        temp_dir.clone().join("the_image").to_str().unwrap()
    );
    assert_eq!(all_images.images, &["the_image".to_string()]);
    config_content_mock.assert();
    d1_mock.assert();
    content_mock.assert();

    // Second load should be entirely offline since it will use the cache.
    config_content_mock.remove();
    d1_mock.remove();
    content_mock.remove();
    let (actual_image, mut all_images) =
        get_next_image(&image_loader, "token".into(), Some(all_images))
            .await
            .ok()
            .unwrap();
    assert_eq!(
        actual_image,
        temp_dir.clone().join("the_image").to_str().unwrap()
    );
    assert_eq!(all_images.images, &["the_image".to_string()]);

    // Make the image list expire: this will cause it to reload, but the image should come from cache.
    let config_content_mock = config_content_mock.create();
    let d1_mock = d1_mock.create();
    all_images.refresh_after = Instant::now();
    let (actual_image, all_images) =
        get_next_image(&image_loader, "token".into(), Some(all_images))
            .await
            .ok()
            .unwrap();
    assert_eq!(
        actual_image,
        temp_dir.clone().join("the_image").to_str().unwrap()
    );
    assert_eq!(all_images.images, &["the_image".to_string()]);
    config_content_mock.assert();
    d1_mock.assert();
}
