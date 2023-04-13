use crate::http::{AppendPaths, Client};
use anyhow::{bail, Context, Result};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

const CLIENT_ID: &str = "9a021cf1-0d67-456b-b821-c1dff53de0e7";
const SCOPE: &str = "offline_access files.read";

pub struct Authenticator {
    client: Client,
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

#[derive(Debug, Eq, PartialEq)]
pub enum AuthMessage {
    HasClientCode(String, String),
    Completed,
}

impl Authenticator {
    pub fn new(sender: Sender<AuthMessage>, base_url: &str) -> Self {
        let base_url = Url::parse(base_url).unwrap();
        Self {
            client: Client::new(),
            access_token: None,
            refresh_token: None,
            sender,
            device_code_url: base_url.append_path("devicecode"),
            token_url: base_url.append_path("token"),
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
                    .with_context(|| "Refresh token")?
            } else {
                let device_response = self
                    .client
                    .post::<DeviceAuthResponse>(
                        self.device_code_url.clone(),
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

#[tokio::test]
async fn auth_then_refresh() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let device_mock = server.mock("POST", "/devicecode")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into())
        ]))
        .with_body(r#"{ "device_code": "dc", "user_code": "uc", "verification_uri": "vu", "interval": 0 } "#)
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
        .with_body(r#"{ "access_token": "ac", "refresh_token": "rt" } "#)
        .expect(1)
        .create();

    let refresh_token_mock = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
            mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            mockito::Matcher::UrlEncoded("scope".into(), SCOPE.into()),
            mockito::Matcher::UrlEncoded("refresh_token".into(), "rt".into()),
        ]))
        .with_body(r#"{ "access_token": "ac2", "refresh_token": "rt2" } "#)
        .expect(1)
        .create();

    let (sender, mut reciever) = tokio::sync::mpsc::channel(8);
    let mut authenticator = Authenticator::new(sender, &url);

    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt");
    assert_eq!(
        reciever.recv().await.unwrap(),
        AuthMessage::HasClientCode("vu".to_string(), "uc".to_string())
    );
    assert_eq!(reciever.recv().await.unwrap(), AuthMessage::Completed);

    device_mock.assert();
    token_mock.assert();
    assert!(!refresh_token_mock.matched());

    authenticator.invalidate_token();
    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac2");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt2");
    device_mock.assert();
    token_mock.assert();
    refresh_token_mock.assert();

    let token = authenticator.get_token().await.unwrap();
    assert_eq!(token, "ac2");
    assert_eq!(authenticator.refresh_token.as_ref().unwrap(), "rt2");
    device_mock.assert();
    token_mock.assert();
    refresh_token_mock.assert();
}

#[cfg(test)]
pub fn test_authenticator() -> Authenticator {
    let (sender, _) = tokio::sync::mpsc::channel(8);
    Authenticator {
        client: Client::new(),
        access_token: Some("token".into()),
        refresh_token: Some("refresh".into()),
        sender,
        device_code_url: Url::parse("http://localhost/devicecode").unwrap(),
        token_url: Url::parse("http://localhost/token").unwrap(),
    }
}
