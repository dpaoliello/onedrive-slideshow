use crate::cred_store;
use crate::http::{AppendPaths, Client};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

const CLIENT_ID: &str = "9a021cf1-0d67-456b-b821-c1dff53de0e7";
const SCOPE: &str = "offline_access files.read";

const REFRESH_TOKEN_PADDING: Duration = Duration::from_secs(60);

pub struct Authenticator {
    client: Client,
    refresh_after: Instant,
    access_token: Option<String>,
    refresh_token: Option<String>,
    sender: Sender<AuthMessage>,
    device_code_url: Url,
    token_url: Url,
}

#[derive(Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponseSuccess {
    expires_in: u64,
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

#[derive(Debug, Eq, PartialEq)]
pub enum AuthMessage {
    HasClientCode(String, String),
    Completed,
}

impl Authenticator {
    pub fn new(sender: Sender<AuthMessage>, base_url: &str, refresh_token: Option<String>) -> Self {
        let base_url = Url::parse(base_url).unwrap();
        Self {
            client: Client::new(),
            refresh_after: Instant::now(),
            access_token: None,
            refresh_token,
            sender,
            device_code_url: base_url.append_path("devicecode"),
            token_url: base_url.append_path("token"),
        }
    }

    pub async fn get_token(&mut self) -> Result<String> {
        if self.access_token.is_none() || Instant::now() > self.refresh_after {
            let response = if let Some(refresh_token) = &self.refresh_token {
                let result = self
                    .client
                    .post::<TokenResponse>(
                        self.token_url.clone(),
                        &[
                            ("client_id", CLIENT_ID),
                            ("grant_type", "refresh_token"),
                            ("scope", SCOPE),
                            ("refresh_token", refresh_token),
                        ],
                        None,
                    )
                    .await
                    .with_context(|| "Refresh token");
                if result.is_err() {
                    // On error, assume we have a bad refresh token.
                    self.refresh_token = None;
                }
                result?
            } else {
                'outer: loop {
                    let device_response = self
                        .client
                        .post::<DeviceAuthResponse>(
                            self.device_code_url.clone(),
                            &[("client_id", CLIENT_ID), ("scope", SCOPE)],
                            None,
                        )
                        .await
                        .with_context(|| "Initial auth request")?;
                    let device_response_expiry = Instant::now()
                        .checked_add(Duration::from_secs(device_response.expires_in))
                        .unwrap();

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
                                self.token_url.clone(),
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
                            match error.as_str() {
                                "authorization_pending" => {
                                    tokio::time::sleep(Duration::from_secs(
                                        device_response.interval,
                                    ))
                                    .await;

                                    if device_response_expiry <= Instant::now() {
                                        // Code has expired, get a new one.
                                        continue 'outer;
                                    } else {
                                        // Check if the user has approved the code.
                                        continue;
                                    }
                                }
                                _ => continue 'outer,
                            }
                        }

                        self.sender.send(AuthMessage::Completed).await.unwrap();
                        break 'outer token_response;
                    }
                }
            };

            match response {
                TokenResponse::Failure(TokenResponseError {
                    error_description, ..
                }) => {
                    bail!(error_description);
                }
                TokenResponse::Success(response) => {
                    self.refresh_after = Duration::from_secs(response.expires_in)
                        .checked_sub(REFRESH_TOKEN_PADDING)
                        .and_then(|expires_in| Instant::now().checked_add(expires_in))
                        .ok_or_else(|| anyhow!("Token expires too quickly"))?;
                    cred_store::store_refresh_token(&response.refresh_token);
                    self.refresh_token = Some(response.refresh_token);
                    self.access_token = Some(response.access_token);
                }
            }
        }

        Ok(self.access_token.as_ref().unwrap().clone())
    }
}

#[tokio::test]
async fn auth_then_refresh() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let device_mock = server.mock("POST", "/devicecode")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into())
        ]))
        .with_body(r#"{ "device_code": "dc", "user_code": "uc", "verification_uri": "vu", "interval": 0, "expires_in": 3600 } "#)
        .expect(1)
        .create();

    let token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded(
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
            mockito::Matcher::UrlEncoded("device_code".into(), "dc".into()),
        ]))
        .with_body(r#"{ "access_token": "ac", "refresh_token": "rt", "expires_in": 60 } "#)
        .expect(1)
        .create();

    let (sender, mut reciever) = tokio::sync::mpsc::channel(8);
    let mut authenticator = Authenticator::new(sender, &url, None);

    // Initial get token.
    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt");
    assert_eq!(
        reciever.try_recv().unwrap(),
        AuthMessage::HasClientCode("vu".to_string(), "uc".to_string())
    );
    assert_eq!(reciever.try_recv().unwrap(), AuthMessage::Completed);

    device_mock.assert();
    token_mock.assert();

    // Token has expired, so we'll have to refresh it.
    let refresh_token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into()),
            mockito::Matcher::UrlEncoded("refresh_token".into(), "rt".into()),
        ]))
        .with_body(r#"{ "access_token": "ac2", "refresh_token": "rt2", "expires_in": 3600 } "#)
        .expect(1)
        .create();
    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac2");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt2");
    device_mock.assert();
    token_mock.assert();
    refresh_token_mock.assert();

    // Token lives for 1hr, so should still be ok.
    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac2");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt2");
    device_mock.assert();
    token_mock.assert();
    refresh_token_mock.assert();
}

#[tokio::test]
async fn device_code_expired() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let first_call = std::sync::atomic::AtomicBool::new(true);
    let device_mock = server.mock("POST", "/devicecode")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into())
        ]))
        .with_body_from_request(move |_| {
            if first_call.swap(false, std::sync::atomic::Ordering::Relaxed) {
                r#"{ "device_code": "dc1", "user_code": "uc1", "verification_uri": "vu1", "interval": 0, "expires_in": 0 } "#.into()
            } else {
                r#"{ "device_code": "dc2", "user_code": "uc2", "verification_uri": "vu2", "interval": 0, "expires_in": 3600 } "#.into()
            }
        })
        .expect(2)
        .create();

    let failed_token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded(
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
            mockito::Matcher::UrlEncoded("device_code".into(), "dc1".into()),
        ]))
        .with_body(r#"{ "error": "expired_token", "error_description": ""}"#)
        .with_status(400)
        .expect(1)
        .create();

    let success_token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded(
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
            mockito::Matcher::UrlEncoded("device_code".into(), "dc2".into()),
        ]))
        .with_body(r#"{ "access_token": "ac", "refresh_token": "rt", "expires_in": 3600 } "#)
        .expect(1)
        .create();

    let (sender, mut reciever) = tokio::sync::mpsc::channel(8);
    let mut authenticator = Authenticator::new(sender, &url, None);
    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt");
    assert_eq!(
        reciever.try_recv().unwrap(),
        AuthMessage::HasClientCode("vu1".to_string(), "uc1".to_string())
    );
    assert_eq!(
        reciever.try_recv().unwrap(),
        AuthMessage::HasClientCode("vu2".to_string(), "uc2".to_string())
    );

    device_mock.assert();
    failed_token_mock.assert();
    success_token_mock.assert();
}

#[tokio::test]
async fn with_existing_refresh_token() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let (sender, mut reciever) = tokio::sync::mpsc::channel(8);
    let mut authenticator = Authenticator::new(sender, &url, Some("rt".to_string()));

    // We have a refresh token, so it should be used.
    let refresh_token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into()),
            mockito::Matcher::UrlEncoded("refresh_token".into(), "rt".into()),
        ]))
        .with_body(r#"{ "access_token": "ac2", "refresh_token": "rt2", "expires_in": 3600 } "#)
        .expect(1)
        .create();
    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac2");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt2");
    assert!(matches!(
        reciever.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    refresh_token_mock.assert();
}

#[tokio::test]
async fn with_existing_but_expired_refresh_token() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let (sender, mut reciever) = tokio::sync::mpsc::channel(8);
    let mut authenticator = Authenticator::new(sender, &url, Some("rt".to_string()));

    // We have a refresh token, so it should be used.
    let refresh_token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into()),
            mockito::Matcher::UrlEncoded("refresh_token".into(), "rt".into()),
        ]))
        .with_body(r#"{ "error": "Refresh token is expired", "error_description": ""}"#)
        .with_status(400)
        .expect(1)
        .create();
    // But it's expired, so there will be a call to the normal flow
    let device_mock = server.mock("POST", "/devicecode")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into())
        ]))
        .with_body(r#"{ "device_code": "dc", "user_code": "uc", "verification_uri": "vu", "interval": 0, "expires_in": 3600 } "#)
        .expect(1)
        .create();
    let token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded(
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
            mockito::Matcher::UrlEncoded("device_code".into(), "dc".into()),
        ]))
        .with_body(r#"{ "access_token": "ac2", "refresh_token": "rt2", "expires_in": 60 } "#)
        .expect(1)
        .create();
    let token = authenticator.get_token().await;
    assert!(token.is_err());
    refresh_token_mock.assert();

    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac2");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt2");
    assert_eq!(
        reciever.try_recv().unwrap(),
        AuthMessage::HasClientCode("vu".to_string(), "uc".to_string())
    );
    assert_eq!(reciever.try_recv().unwrap(), AuthMessage::Completed);
    device_mock.assert();
    token_mock.assert();
    refresh_token_mock.assert();
}
