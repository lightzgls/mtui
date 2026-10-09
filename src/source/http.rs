//! Shared HTTP transport for YouTube Music metadata endpoints.

use std::time::Duration;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};

#[derive(Clone)]
pub struct Cancellation { generation: Arc<AtomicU64>, expected: u64 }
impl Cancellation {
    pub fn capture(generation: Arc<AtomicU64>) -> Self {
        let expected = generation.load(Ordering::SeqCst);
        Self { generation, expected }
    }
    pub fn active(&self) -> bool { self.expected == self.generation.load(Ordering::SeqCst) }
    pub async fn run<T>(&self, task: impl std::future::Future<Output=Result<T>>) -> Result<T> {
        let mut task = std::pin::pin!(task);
        loop {
            if !self.active() { anyhow::bail!("request was superseded"); }
            if let Ok(result) = tokio::time::timeout(Duration::from_millis(40), &mut task).await { return result; }
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A reusable client and runtime for synchronous source workers.
pub struct Http {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    cancel: Option<Cancellation>,
}

impl Http {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime for YouTube Music calls")?;
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .context("could not build the YouTube Music HTTP client")?;
        Ok(Self { client, runtime, cancel: None })
    }

    pub fn set_cancellation(&mut self, cancel: Option<Cancellation>) { self.cancel = cancel; }

    pub(super) fn send(&self, request: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>)> {
        let task = async {
            let response = request.send().await?;
            let status = response.status().as_u16();
            let bytes = super::body::read(response, MAX_BODY_BYTES).await?;
            Ok((status, bytes))
        };
        self.runtime.block_on(async {
            if let Some(cancel) = &self.cancel { cancel.run(task).await }
            else { task.await }
        })
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Optional category reads share a pool and run two at a time. Parse each
    /// bounded body immediately so raw responses do not accumulate in memory.
    pub(super) fn send_many<T>(&self, requests: Vec<reqwest::RequestBuilder>, parse: impl Fn(usize, u16, &[u8]) -> Result<T>) -> Result<Vec<Result<T>>> {
        let task = async {
            let results = stream::iter(requests.into_iter().enumerate()).map(|(index, request)| {
                let parse = &parse;
                async move {
                    let response = request.send().await?;
                    let status = response.status().as_u16();
                    let body = super::body::read(response, MAX_BODY_BYTES).await?;
                    parse(index, status, &body)
                }
            }).buffered(2).collect().await;
            Ok(results)
        };
        self.runtime.block_on(async {
            if let Some(cancel) = &self.cancel { cancel.run(task).await } else { task.await }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::test_http::{serve, immediate};
    #[test]
    fn obsolete_reads_cancel_during_the_response_body() {
        let (url, server) = serve(vec![(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nx".to_vec(), Duration::from_secs(2))]);
        let generation = Arc::new(AtomicU64::new(0));
        let mut http = Http::new().unwrap();
        http.set_cancellation(Some(Cancellation::capture(generation.clone())));
        let change = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            generation.fetch_add(1, Ordering::SeqCst);
        });
        let start = std::time::Instant::now();
        let error = http.send(http.client().get(url)).unwrap_err();
        assert!(error.to_string().contains("superseded"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(1));
        change.join().unwrap(); server.join().unwrap();
    }
    #[test]
    fn oversized_metadata_is_rejected_before_buffering() {
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", MAX_BODY_BYTES + 1);
        let (url, server) = serve(vec![immediate(response.as_bytes())]);
        let http = Http::new().unwrap();
        assert!(http.send(http.client().get(url)).unwrap_err().to_string().contains("limit"));
        server.join().unwrap();
    }
}
