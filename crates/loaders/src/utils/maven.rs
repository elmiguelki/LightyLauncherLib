// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! Shared HEAD/sidecar helpers for Maven artifact metadata.

use lighty_core::hosts::{HTTP_CLIENT as CLIENT, build_fallback_urls};

/// Fetches the expected SHA1 of a Maven artifact from its `.sha1` sidecar,
/// trying every mirror of `jar_url` in turn.
///
/// The Forge-family CDNs (Cloudflare in front of JFrog) strip custom
/// checksum headers, so the sidecar is the only authoritative source.
pub async fn fetch_maven_sha1(jar_url: &str) -> Option<String> {
    for candidate in build_fallback_urls(jar_url) {
        let sha1_url = format!("{}.sha1", candidate);

        let Ok(response) = CLIENT.get(&sha1_url).send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }

        let sha1 = response.text().await.ok().and_then(|text| {
            let sha1 = text.split_whitespace().next()?.to_string();
            (sha1.len() == 40).then_some(sha1)
        });
        if sha1.is_some() {
            return sha1;
        }
    }

    None
}

/// Returns a remote file's size without downloading the body (HEAD request),
/// trying every mirror of `url` in turn.
pub async fn fetch_file_size(url: &str) -> Option<u64> {
    for candidate in build_fallback_urls(url) {
        let Ok(response) = CLIENT.head(&candidate).send().await else {
            continue;
        };

        let size = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.parse().ok());
        if size.is_some() {
            return size;
        }
    }

    None
}

/// Fetches `(sha1, size)` in parallel for a single Maven artifact URL.
pub async fn fetch_maven_metadata(url: &str) -> (Option<String>, Option<u64>) {
    tokio::join!(fetch_maven_sha1(url), fetch_file_size(url))
}

/// Probes a list of Maven bases and returns the first one that serves
/// `relative_path` with a non-zero `Content-Length`.
///
/// `bases` must already have a trailing `/`.
pub async fn probe_maven_bases(bases: &[&str], relative_path: &str) -> Option<String> {
    for base in bases {
        let url = format!("{}{}", base, relative_path);
        if let Ok(resp) = CLIENT.head(&url).send().await {
            if resp.status().is_success() {
                // Some CDNs answer 200 with an empty body when the
                // artifact is missing — treat zero-length as not-found.
                let len = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                if len > 0 {
                    return Some(url);
                }
            }
        }
    }
    None
}
