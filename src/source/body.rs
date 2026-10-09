//! Limits decoded HTTP bodies while they arrive, including unknown-length responses.

use anyhow::{Result, bail};

pub async fn read(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|length| length > limit as u64) {
        bail!("response exceeds the {} KiB limit", limit / 1024);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            bail!("response exceeds the {} KiB limit", limit / 1024);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::test_http::{serve, immediate};
    #[test]
    fn unknown_length_bodies_are_limited_while_reading() {
        let (url, server) = serve(vec![immediate(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n40\r\n0123456789012345678901234567890123456789012345678901234567890123\r\n0\r\n\r\n")]);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(async {
            let response = reqwest::Client::builder().no_proxy().build().unwrap().get(url).send().await.unwrap();
            read(response, 32).await.unwrap_err()
        });
        assert!(error.to_string().contains("limit"), "{error:#}");
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn compressed_bodies_are_limited_after_decompression() {
        let compressed = [31,139,8,0,0,0,0,0,2,10,171,168,24,5,163,96,20,12,119,0,0,230,201,65,59,232,3,0,0];
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", compressed.len()).into_bytes();
        response.extend_from_slice(&compressed);
        let (url, server) = serve(vec![(response, std::time::Duration::ZERO)]);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(async {
            let response = reqwest::Client::builder().no_proxy().build().unwrap().get(url).send().await.unwrap();
            read(response, 64).await.unwrap_err()
        });
        assert!(error.to_string().contains("limit"), "{error:#}");
        server.join().unwrap();
    }
}
