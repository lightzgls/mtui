//! Shared HTTP transport for YouTube Music metadata endpoints.

use std::time::Duration;

use anyhow::{Context, Result};

const TIMEOUT: Duration = Duration::from_secs(20);

/// A reusable client and runtime for synchronous source workers.
pub struct Http {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Http {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime for YouTube Music calls")?;
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("could not build the YouTube Music HTTP client")?;
        Ok(Self { client, runtime })
    }

    pub(super) fn send(&self, request: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>)> {
        self.runtime.block_on(async {
            let response = request.send().await?;
            let status = response.status().as_u16();
            let bytes = response.bytes().await?;
            Ok((status, bytes.to_vec()))
        })
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }
}
