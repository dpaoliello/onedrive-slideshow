use std::{error::Error, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use reqwest::{RequestBuilder, Response, StatusCode, Url};

pub struct Client {
    inner: reqwest::Client,
}

impl Client {
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::builder().gzip(true).build().unwrap(),
        }
    }

    fn should_retry(response: &reqwest::Result<Response>) -> bool {
        match response {
            Ok(response) => {
                // Retry on server error
                response.status().is_server_error()
            }
            Err(err) => {
                // Retry on timeout.
                if err.is_timeout() {
                    return true;
                }

                let mut source = err.source();
                while let Some(err) = source {
                    if let Some(err) = err.downcast_ref::<std::io::Error>() {
                        match err.raw_os_error() {
                            // Retry on DNS lookup failure.
                            #[cfg(windows)]
                            Some(windows_sys::Win32::Networking::WinSock::WSAHOST_NOT_FOUND) => {
                                return true;
                            }
                            _ => break,
                        }
                    } else {
                        source = err.source();
                    }
                }

                false
            }
        }
    }

    async fn send_with_retry(
        &self,
        make_request: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> reqwest::Result<Response> {
        const MAX_RETRIES: u32 = 5;
        const RETRY_DELAY: Duration = if cfg!(test) {
            Duration::from_millis(5)
        } else {
            Duration::from_millis(500)
        };
        let mut retries = 0;

        loop {
            let response = make_request(&self.inner).send().await;

            if retries < MAX_RETRIES && Client::should_retry(&response) {
                tokio::time::sleep(RETRY_DELAY.saturating_mul(retries)).await;
                retries += 1;
            } else {
                break response;
            }
        }
    }

    pub async fn get<T>(&self, token: &str, url: Url) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let raw_response = self
            .send_with_retry(|client| client.get(url.clone()).bearer_auth(token))
            .await
            .with_context(|| "Sending request failed")?;

        match raw_response.error_for_status_ref() {
            Ok(_) => raw_response
                .json::<T>()
                .await
                .with_context(|| "Parsing response failed"),
            Err(err) => Err(err).context(format!(
                "Response: {}",
                raw_response.text().await.unwrap_or_default()
            )),
        }
    }

    pub async fn post<T>(
        &self,
        url: Url,
        parameters: &[(&str, &str)],
        expected_error: Option<StatusCode>,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = self
            .send_with_retry(|client| client.post(url.clone()).form(parameters))
            .await
            .with_context(|| "Sending request failed")?;

        let response = match expected_error {
            Some(expected_error) if response.status() == expected_error => response,
            _ => response.error_for_status()?,
        };

        response
            .json::<T>()
            .await
            .with_context(|| "Parsing response failed")
    }

    pub async fn download(&self, token: &str, url: Url) -> Result<Bytes> {
        Ok(self
            .send_with_retry(|client| client.get(url.clone()).bearer_auth(token))
            .await
            .with_context(|| "Sending request failed")?
            .bytes()
            .await?)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_after_server_error() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let fail_mock = server
        .mock("GET", "/error")
        .with_status(500)
        .expect(1)
        .create();
    let success_mock = server
        .mock("GET", "/success")
        .with_status(200)
        .expect(1)
        .create();

    let client = Client::new();
    let response = client
        .send_with_retry(|client| {
            let url = if !fail_mock.matched() {
                format!("{url}/error")
            } else {
                format!("{url}/success")
            };
            client.get(url)
        })
        .await;

    fail_mock.assert();
    success_mock.assert();
    assert_eq!(response.unwrap().status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_always_error() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let mock = server.mock("GET", "/").with_status(500).expect(6).create();

    let client = Client::new();
    let response = client.send_with_retry(|client| client.get(&url)).await;

    assert_eq!(response.unwrap().status(), 500);
    mock.assert();
}

pub trait AppendPaths {
    fn append_path(&self, path: &str) -> Self;
    fn append_paths(&self, paths: &[&str]) -> Self;
}

impl AppendPaths for Url {
    fn append_path(&self, path: &str) -> Self {
        let mut new_url = self.clone();
        new_url.path_segments_mut().unwrap().push(path);
        new_url
    }

    fn append_paths(&self, paths: &[&str]) -> Self {
        let mut new_url = self.clone();
        new_url.path_segments_mut().unwrap().extend(paths);
        new_url
    }
}
