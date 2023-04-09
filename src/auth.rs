use crate::http::Client;
use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

const CLIENT_ID: &str = "9a021cf1-0d67-456b-b821-c1dff53de0e7";
const SCOPE: &str = "offline_access files.read";

const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";

pub struct Authenticator {
    client: Client,
    access_token: Option<String>,
    refresh_token: Option<String>,
    sender: Sender<AuthMessage>,
}

#[derive(Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: i32,
}

#[derive(Deserialize)]
struct TokenResponseSuccess {
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct TokenResponseError {
    error: String,
    error_description: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TokenResponse {
    Success(TokenResponseSuccess),
    Failure(TokenResponseError),
}

#[derive(Debug)]
pub enum AuthMessage {
    HasClientCode(String, String),
    Completed,
}

impl Authenticator {
    pub fn new(sender: Sender<AuthMessage>) -> Self {
        Self {
            client: Client::new(),
            access_token: None,
            refresh_token: None,
            sender,
        }
    }

    pub fn invalidate_token(&mut self) {
        self.access_token = None;
    }

    pub async fn get_token(&mut self) -> Result<&str> {
        if self.access_token.is_none() {
            let response = if let Some(refresh_token) = &self.refresh_token {
                self.client
                    .post::<TokenResponse>(
                        TOKEN_URL,
                        &[
                            ("client_id", CLIENT_ID),
                            ("grant_type", "refresh_token"),
                            ("scope", SCOPE),
                            ("refresh_token", refresh_token),
                        ],
                        None,
                    )
                    .await
                    .with_context(|| "Refresh token")?
            } else {
                let device_response = self
                    .client
                    .post::<DeviceAuthResponse>(
                        DEVICE_CODE_URL,
                        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
                        None,
                    )
                    .await
                    .with_context(|| "Initial auth request")?;

                self.sender
                    .send(AuthMessage::HasClientCode(
                        device_response.verification_uri,
                        device_response.user_code,
                    ))
                    .await
                    .unwrap();

                loop {
                    let token_response = self
                        .client
                        .post::<TokenResponse>(
                            TOKEN_URL,
                            &[
                                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                                ("client_id", CLIENT_ID),
                                ("device_code", &device_response.device_code),
                            ],
                            Some(StatusCode::BAD_REQUEST),
                        )
                        .await
                        .with_context(|| "Exchange token")?;

                    if let TokenResponse::Failure(TokenResponseError { error, .. }) =
                        &token_response
                    {
                        if error == "authorization_pending" {
                            tokio::time::sleep(Duration::from_secs(
                                device_response.interval as u64,
                            ))
                            .await;
                            continue;
                        }
                    }

                    self.sender.send(AuthMessage::Completed).await.unwrap();
                    break token_response;
                }
            };

            match response {
                TokenResponse::Failure(TokenResponseError {
                    error_description, ..
                }) => {
                    bail!(error_description);
                }
                TokenResponse::Success(response) => {
                    self.refresh_token = Some(response.refresh_token);
                    self.access_token = Some(response.access_token);
                }
            }
        }

        Ok(self.access_token.as_ref().unwrap())
    }
}
