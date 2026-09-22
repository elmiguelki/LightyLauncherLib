// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! File download utilities with retry logic and concurrency control

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "events")]
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::fs;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Semaphore;
use futures::future::try_join_all;
use futures::StreamExt;
use lighty_core::hosts::{HTTP_CLIENT as CLIENT, build_fallback_urls};
use lighty_core::mkdir;
use crate::errors::InstallerResult;
use crate::errors::InstallerError;
use super::config::get_config;

#[cfg(feature = "events")]
use lighty_event::{EventBus, Event, LaunchEvent};

#[cfg(feature = "events")]
#[derive(Copy, Clone)]
pub(crate) enum DownloadProgressKind {
    Assets,
    Libraries,
    Natives,
    Mods,
}

#[cfg(feature = "events")]
#[derive(Clone)]
struct ProgressState<'a> {
    bus: &'a EventBus,
    kind: DownloadProgressKind,
    total: usize,
    current: Arc<AtomicUsize>,
}

#[cfg(feature = "events")]
impl<'a> ProgressState<'a> {
    fn emit_progress(&self) {
        let current = self.current.fetch_add(1, Ordering::Relaxed) + 1;
        let event = match self.kind {
            DownloadProgressKind::Assets => LaunchEvent::DownloadingAssets {
                current,
                total: self.total,
            },
            DownloadProgressKind::Libraries => LaunchEvent::DownloadingLibraries {
                current,
                total: self.total,
            },
            DownloadProgressKind::Natives => LaunchEvent::DownloadingNatives {
                current,
                total: self.total,
            },
            DownloadProgressKind::Mods => LaunchEvent::DownloadingMods {
                current,
                total: self.total,
            },
        };

        self.bus.emit(Event::Launch(event));
    }
}

/// Calculate exponential backoff delay with jitter to prevent thundering herd
fn calculate_retry_delay(base_delay_ms: u64, attempt: u32) -> u64 {
    let exponential_delay = base_delay_ms * 2u64.pow(attempt - 1);
    // Add jitter: random value between 0% and 50% of the delay
    let jitter = fastrand::u64(0..=exponential_delay / 2);
    exponential_delay + jitter
}

/// Downloads small files (loaded entirely in memory)
pub async fn download_small_file(
    url: String,
    dest: PathBuf,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    let config = get_config();
    let mut last_error = None;
    let candidates = build_fallback_urls(&url);

    for attempt in 1..=config.max_retries {
        for candidate in &candidates {
            match download_small_file_once(
                candidate,
                &dest,
                #[cfg(feature = "events")]
                event_bus,
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_error = Some(e);
                    if candidates.len() > 1 && candidate != &url {
                        lighty_core::trace_warn!(
                            "[Mirror] Failed to download via {}: {}",
                            candidate,
                            last_error.as_ref().unwrap()
                        );
                    }
                }
            }
        }

        if attempt < config.max_retries {
            let delay = calculate_retry_delay(config.initial_delay_ms, attempt);
            lighty_core::trace_warn!(
                "[Retry {}/{}] Failed to download {} after {} mirror(s). Retrying in {}ms...",
                attempt,
                config.max_retries,
                url,
                candidates.len(),
                delay
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        InstallerError::DownloadFailed(format!(
            "Download failed after {} retries without specific error details: {}",
            config.max_retries, url
        ))
    }))
}

async fn download_small_file_once(
    url: &str,
    dest: &PathBuf,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    let resp = CLIENT.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(InstallerError::DownloadFailed(format!(
            "HTTP {} for {}",
            resp.status(),
            url
        )));
    }
    let bytes = resp.bytes().await?;

    let dest_str = dest.to_string_lossy().to_lowercase();
    if dest_str.ends_with(".jar") || dest_str.ends_with(".zip") {
        if bytes.len() < 4 || bytes[0] != 0x50 || bytes[1] != 0x4B {
            return Err(InstallerError::DownloadFailed(format!(
                "Invalid JAR/ZIP response (magic mismatch or HTML anti-bot page) from {}",
                url
            )));
        }
    }

    // Emit install progress event for the entire file
    #[cfg(feature = "events")]
    if let Some(bus) = event_bus {
        bus.emit(Event::Launch(LaunchEvent::InstallProgress {
            bytes: bytes.len() as u64,
        }));
    }

    if let Some(parent) = dest.parent() {
        mkdir!(parent);
    }

    fs::write(dest, bytes).await?;
    Ok(())
}

/// Downloads large files with streaming (memory efficient)
pub async fn download_large_file(
    url: String,
    dest: PathBuf,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    let config = get_config();
    let mut last_error = None;
    let candidates = build_fallback_urls(&url);

    for attempt in 1..=config.max_retries {
        for candidate in &candidates {
            match download_large_file_once(
                candidate,
                &dest,
                #[cfg(feature = "events")]
                event_bus,
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_error = Some(e);
                    if candidates.len() > 1 && candidate != &url {
                        lighty_core::trace_warn!(
                            "[Mirror] Failed to download via {}: {}",
                            candidate,
                            last_error.as_ref().unwrap()
                        );
                    }
                }
            }
        }

        if attempt < config.max_retries {
            let delay = calculate_retry_delay(config.initial_delay_ms, attempt);
            lighty_core::trace_warn!(
                "[Retry {}/{}] Failed to download {} after {} mirror(s). Retrying in {}ms...",
                attempt,
                config.max_retries,
                url,
                candidates.len(),
                delay
            );
            let _ = fs::remove_file(&dest).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        InstallerError::DownloadFailed(format!(
            "Download failed after {} retries without specific error details: {}",
            config.max_retries, url
        ))
    }))
}

async fn download_large_file_once(
    url: &str,
    dest: &PathBuf,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    let response = CLIENT.get(url).send().await?;

    if !response.status().is_success() {
        return Err(InstallerError::DownloadFailed(format!(
            "HTTP {} for {}",
            response.status(),
            url
        )));
    }

    if let Some(parent) = dest.parent() {
        mkdir!(parent);
    }

    let file = fs::File::create(dest).await?;
    let mut writer = BufWriter::with_capacity(256 * 1024, file);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        writer.write_all(&chunk).await?;

        // Emit install progress event for this chunk
        #[cfg(feature = "events")]
        if let Some(bus) = event_bus {
            bus.emit(Event::Launch(LaunchEvent::InstallProgress {
                bytes: chunk.len() as u64,
            }));
        }
    }

    writer.flush().await?;
    Ok(())
}

/// Downloads multiple large files with concurrency limit
pub async fn download_with_concurrency_limit(
    tasks: Vec<(String, PathBuf)>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
    #[cfg(feature = "events")] progress_kind: Option<DownloadProgressKind>,
) -> InstallerResult<()> {
    let config = get_config();
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_downloads));
    #[cfg(feature = "events")]
    let progress = match (event_bus, progress_kind) {
        (Some(bus), Some(kind)) if !tasks.is_empty() => Some(ProgressState {
            bus,
            kind,
            total: tasks.len(),
            current: Arc::new(AtomicUsize::new(0)),
        }),
        _ => None,
    };
    let futures: Vec<_> = tasks
        .into_iter()
        .map(|(url, dest)| {
            let sem = semaphore.clone();
            #[cfg(feature = "events")]
            let progress = progress.clone();
            async move {
                let _permit = sem.acquire().await
                    .map_err(|_| InstallerError::DownloadFailed(
                        "Download concurrency semaphore closed".into()
                    ))?;
                download_large_file(
                    url,
                    dest,
                    #[cfg(feature = "events")]
                    event_bus,
                )
                .await?;

                #[cfg(feature = "events")]
                if let Some(progress) = &progress {
                    progress.emit_progress();
                }

                Ok::<(), InstallerError>(())
            }
        })
        .collect();

    try_join_all(futures).await?;
    Ok(())
}

/// Downloads multiple small files with concurrency limit
pub async fn download_small_with_concurrency_limit(
    tasks: Vec<(String, PathBuf)>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
    #[cfg(feature = "events")] progress_kind: Option<DownloadProgressKind>,
) -> InstallerResult<()> {
    let config = get_config();
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_downloads));
    #[cfg(feature = "events")]
    let progress = match (event_bus, progress_kind) {
        (Some(bus), Some(kind)) if !tasks.is_empty() => Some(ProgressState {
            bus,
            kind,
            total: tasks.len(),
            current: Arc::new(AtomicUsize::new(0)),
        }),
        _ => None,
    };
    let futures: Vec<_> = tasks
        .into_iter()
        .map(|(url, dest)| {
            let sem = semaphore.clone();
            #[cfg(feature = "events")]
            let progress = progress.clone();
            async move {
                let _permit = sem.acquire().await
                    .map_err(|_| InstallerError::DownloadFailed(
                        "Download concurrency semaphore closed".into()
                    ))?;
                download_small_file(
                    url,
                    dest,
                    #[cfg(feature = "events")]
                    event_bus,
                )
                .await?;

                #[cfg(feature = "events")]
                if let Some(progress) = &progress {
                    progress.emit_progress();
                }

                Ok::<(), InstallerError>(())
            }
        })
        .collect();

    try_join_all(futures).await?;
    Ok(())
}
