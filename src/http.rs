use anyhow::{Context, Result};
use reqwest::StatusCode;
use std::ops::Deref;

use crate::auth::Authenticator;

pub struct Client {
    inner: reqwest::Client,
}

impl Client {
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::builder().gzip(true).build().unwrap(),
        }
    }

    pub async fn get<T>(&self, authenticator: &mut Authenticator, url: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        loop {
            let response = self
                .inner
                .get(url)
                .bearer_auth(authenticator.get_token().await?)
                .send()
                .await
                .with_context(|| "Sending request failed")?
                .error_for_status();

            match response {
                Ok(response) => {
                    return response
                        .json::<T>()
                        .await
                        .with_context(|| "Parsing response failed")
                }
                Err(err) if err.status() == Some(StatusCode::UNAUTHORIZED) => {
                    authenticator.invalidate_token()
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    pub async fn post<T>(
        &self,
        url: &str,
        parameters: &[(&str, &str)],
        expected_error: Option<StatusCode>,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = self
            .inner
            .post(url)
            .form(parameters)
            .send()
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

    pub async fn download(
        &self,
        authenticator: &mut Authenticator,
        url: &str,
    ) -> Result<impl Deref<Target = [u8]>> {
        Ok(self
            .inner
            .get(url)
            .bearer_auth(authenticator.get_token().await?)
            .send()
            .await
            .with_context(|| "Sending request failed")?
            .bytes()
            .await?)
    }
}