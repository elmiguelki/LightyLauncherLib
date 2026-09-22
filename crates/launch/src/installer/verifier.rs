// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! File verification and cache checking utilities

use std::path::PathBuf;
use tokio::fs;
use lighty_core::verify_file_sha1;

/// Returns whether the file at `path` needs to be (re-)downloaded.
///
/// `true` if the file is missing, zero-byte (corrupt — a stale empty
/// stub from a previously-failed download), or the SHA1 doesn't match
/// `sha1`. `false` if the file exists, has non-zero size, and either
/// has no expected hash or matches the expected one.
pub async fn needs_download(path: &PathBuf, sha1: Option<&String>, name: &str) -> bool {
    if !path.exists() {
        return true;
    }

    // Empty files are a stale artifact of a previous failed download
    // (server returned 200 with empty body, or the writer crashed mid-stream).
    if let Ok(meta) = fs::metadata(path).await {
        if meta.len() == 0 {
            lighty_core::trace_warn!(
                "[Installer] Zero-byte cached file for {}, re-downloading...",
                name
            );
            let _ = fs::remove_file(path).await;
            return true;
        }

        // Validate JAR/ZIP files have valid magic bytes (PK..) and aren't HTML error pages
        let path_str = path.to_string_lossy().to_lowercase();
        if path_str.ends_with(".jar") || path_str.ends_with(".zip") {
            use tokio::io::AsyncReadExt;
            if let Ok(mut f) = fs::File::open(path).await {
                let mut magic = [0u8; 4];
                if let Ok(n) = f.read(&mut magic).await {
                    if n < 4 || magic[0] != 0x50 || magic[1] != 0x4B {
                        lighty_core::trace_warn!(
                            "[Installer] Corrupt JAR/ZIP (invalid magic bytes or HTML error page) for {}, re-downloading...",
                            name
                        );
                        drop(f);
                        let _ = fs::remove_file(path).await;
                        return true;
                    }
                }
            }
        }
    }

    if let Some(hash) = sha1 {
        match verify_file_sha1(path, hash).await {
            Ok(true) => false,
            _ => {
                lighty_core::trace_warn!("[Installer] SHA1 mismatch for {}, re-downloading...", name);
                let _ = fs::remove_file(path).await;
                true
            }
        }
    } else {
        false
    }
}
