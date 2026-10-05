// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! HTTP download helpers built on top of the shared `HTTP_CLIENT`.

use std::path::Path;

use tokio::fs;
use tokio::io::AsyncWriteExt;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING};

use crate::errors::{DownloadError, DownloadResult};
use crate::hosts::{HTTP_CLIENT, RAW_HTTP_CLIENT, build_fallback_urls};
use crate::trace_debug;

/// Streams `url` to `path` chunk by chunk. No progress callback —
/// reserved for small artefacts (modpack archives, single mod files).
pub async fn download_file_untracked(url: &str, path: impl AsRef<Path>) -> DownloadResult<()> {
    let path = path.as_ref().to_owned();
    let mut last_error = None;

    for candidate in build_fallback_urls(url) {
        match download_untracked_once(&HTTP_CLIENT, &candidate, &path).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                let should_retry_raw = is_decode_error(&e);
                last_error = Some(e);

                if should_retry_raw {
                    trace_debug!(
                        url = %candidate,
                        "Response decode failed; retrying download without automatic decompression"
                    );

                    match download_untracked_once(&RAW_HTTP_CLIENT, &candidate, &path).await {
                        Ok(_) => return Ok(()),
                        Err(raw_err) => {
                            last_error = Some(raw_err);
                        }
                    }
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        DownloadError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "No candidates available for download",
        ))
    }))
}

fn is_decode_error(err: &DownloadError) -> bool {
    match err {
        DownloadError::Http(http_err) => http_err.is_decode(),
        _ => false,
    }
}

async fn download_untracked_once(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
) -> DownloadResult<()> {
    let mut response = client
        .get(url)
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await?
        .error_for_status()?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut file = fs::File::create(path).await?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Downloads `url` into an in-memory buffer and reports progress after
/// every chunk via `on_progress(downloaded_bytes, total_bytes)`. The
/// total is `0` when the server doesn't expose `Content-Length`.
pub async fn download_file<F>(url: &str, on_progress: F) -> DownloadResult<Vec<u8>>
where
    F: Fn(u64, u64),
{
    let trimmed = url.trim();
    trace_debug!("Downloading {trimmed}");

    let mut last_error = None;

    for candidate in build_fallback_urls(url) {
        let mut response = match download_streaming_once(&HTTP_CLIENT, candidate.trim()).await {
            Ok(response) => response,
            Err(e) => {
                if is_decode_error(&e) {
                    trace_debug!(
                        url = %candidate,
                        "Response decode failed; retrying streaming download without automatic decompression"
                    );
                    match download_streaming_once(&RAW_HTTP_CLIENT, candidate.trim()).await {
                        Ok(response) => response,
                        Err(raw_err) => {
                            last_error = Some(raw_err);
                            continue;
                        }
                    }
                } else {
                    last_error = Some(e);
                    continue;
                }
            }
        };

        trace_debug!("Response received from url");

        let encoding = response
            .headers()
            .get(CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("identity");
        let is_identity = encoding.eq_ignore_ascii_case("identity");
        let max_len = if is_identity {
            response.content_length().unwrap_or(0)
        } else {
            0
        };
        let mut output = Vec::with_capacity(max_len as usize);
        let mut curr_len = 0;

        on_progress(0, max_len);

        trace_debug!("Reading data from response chunk...");
        while let Some(data) = response.chunk().await? {
            output.extend_from_slice(&data);
            curr_len += data.len();
            if max_len > 0 {
                let capped = (curr_len as u64).min(max_len);
                on_progress(capped, max_len);
            } else {
                on_progress(curr_len as u64, max_len);
            }
        }

        trace_debug!("Downloaded file");
        return Ok(output);
    }

    Err(last_error.unwrap_or_else(|| {
        DownloadError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "No candidates available for download",
        ))
    }))
}

async fn download_streaming_once(
    client: &reqwest::Client,
    url: &str,
) -> DownloadResult<reqwest::Response> {
    Ok(client
        .get(url)
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await?
        .error_for_status()?)
}
