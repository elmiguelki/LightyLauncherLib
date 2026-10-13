// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! File download utilities with retry logic and concurrency control

use std::path::{Path, PathBuf};
#[cfg(feature = "events")]
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use reqwest::header::RANGE;
use reqwest::StatusCode;
use sha1::{Digest, Sha1};
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::Semaphore;
use futures::future::join_all;
use futures::StreamExt;
use lighty_core::calculate_sha1_bytes;
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

/// One file to fetch. `url` and `sha1` borrow the manifest that is alive for
/// the whole install, so building the list copies no strings; only `dest` is
/// owned, being a path that exists nowhere else.
pub struct DownloadTask<'a> {
    pub url: &'a str,
    pub dest: PathBuf,
    pub sha1: Option<&'a str>,
    pub size: u64,
}

/// Exponential backoff with up-to-50% jitter to prevent thundering herd.
fn calculate_retry_delay(base_delay_ms: u64, attempt: u32) -> u64 {
    let exponential_delay = base_delay_ms * 2u64.pow(attempt - 1);
    let jitter = fastrand::u64(0..=exponential_delay / 2);
    exponential_delay + jitter
}

/// Renders an error with its source chain: reqwest's own message stops at
/// "error sending request" and leaves the timeout or reset in the source.
fn describe(error: &InstallerError) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);

    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // `#[from]` variants already print their source in their own message.
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }

    text
}

/// Last failure per source, so the final error names the official host
/// rather than whichever mirror happened to be tried last.
#[derive(Default)]
struct SourceFailures {
    primary: Option<InstallerError>,
    mirror: Option<InstallerError>,
}

impl SourceFailures {
    fn record(&mut self, candidate: &str, url: &str, error: InstallerError) {
        if candidate == url {
            self.primary = Some(error);
        } else {
            lighty_core::trace_warn!(
                "[Mirror] Failed to download via {}: {}",
                candidate,
                describe(&error)
            );
            self.mirror = Some(error);
        }
    }

    fn into_error(self, url: &str, attempts: u32) -> InstallerError {
        let reason = match (&self.primary, &self.mirror) {
            (Some(primary), Some(mirror)) => {
                format!("{}; mirror: {}", describe(primary), describe(mirror))
            }
            (Some(error), None) | (None, Some(error)) => describe(error),
            (None, None) => "no attempt was made".to_string(),
        };

        InstallerError::RetriesExhausted {
            attempts,
            url: url.to_string(),
            reason,
        }
    }
}

/// Downloads small files (loaded entirely in memory)
pub async fn download_small_file(
    url: &str,
    dest: &Path,
    sha1: Option<&str>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    let config = get_config();
    let mut failures = SourceFailures::default();
    let candidates = build_fallback_urls(url);

    for attempt in 1..=config.max_retries {
        for candidate in &candidates {
            match download_small_file_once(
                candidate,
                dest,
                sha1,
                #[cfg(feature = "events")]
                event_bus,
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(e) => failures.record(candidate, url, e),
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

    Err(failures.into_error(url, config.max_retries))
}

async fn download_small_file_once(
    url: &str,
    dest: &Path,
    sha1: Option<&str>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    let response = CLIENT.get(url).send().await?;

    if !response.status().is_success() {
        return Err(InstallerError::HttpStatus {
            status: response.status().as_u16(),
            url: url.to_string(),
        });
    }

    let bytes = response.bytes().await?;

    // Mirrors and anti-bot pages answer 200 with HTML; never let that land as a jar.
    if is_zip_like(dest) && !has_zip_magic(&bytes) {
        return Err(InstallerError::InvalidArchive {
            url: url.to_string(),
        });
    }

    if let Some(expected) = sha1 {
        let digest = calculate_sha1_bytes(&bytes);

        if !digest.eq_ignore_ascii_case(expected) {
            return Err(InstallerError::Sha1Mismatch {
                url: url.to_string(),
                expected: expected.to_string(),
                actual: digest,
            });
        }
    }

    #[cfg(feature = "events")]
    if let Some(bus) = event_bus {
        bus.emit(Event::Launch(LaunchEvent::InstallProgress {
            bytes: bytes.len() as u64,
        }));
    }

    if let Some(parent) = dest.parent() {
        mkdir!(parent);
    }

    // Rename into place: the object store trusts any non-empty file, so a
    // write cut short by a crash would otherwise pass for a valid asset.
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);

    fs::write(&partial, bytes).await?;
    fs::rename(&partial, dest).await?;
    Ok(())
}

/// Downloads large files with streaming (memory efficient)
pub async fn download_large_file(
    url: &str,
    dest: &Path,
    sha1: Option<&str>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    let config = get_config();
    let mut failures = SourceFailures::default();
    let candidates = build_fallback_urls(url);

    for attempt in 1..=config.max_retries {
        for candidate in &candidates {
            match download_large_file_once(
                candidate,
                dest,
                sha1,
                #[cfg(feature = "events")]
                event_bus,
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(e) => failures.record(candidate, url, e),
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

    Err(failures.into_error(url, config.max_retries))
}

async fn download_large_file_once(
    url: &str,
    dest: &Path,
    sha1: Option<&str>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()> {
    // Resume what a previous attempt left behind: a dropped connection then
    // costs neither the bytes already received nor a second progress count.
    let downloaded = fs::metadata(dest).await.map(|meta| meta.len()).unwrap_or(0);

    let mut request = CLIENT.get(url);
    if downloaded > 0 {
        request = request.header(RANGE, format!("bytes={}-", downloaded));
    }

    let response = request.send().await?;
    let status = response.status();

    // The partial outgrew the file itself; drop it so the next attempt starts clean.
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        let _ = fs::remove_file(dest).await;
        return Err(InstallerError::StalePartialFile {
            url: url.to_string(),
        });
    }

    if !status.is_success() {
        return Err(InstallerError::HttpStatus {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }

    if let Some(parent) = dest.parent() {
        mkdir!(parent);
    }

    let mut hasher = Sha1::new();

    // Only 206 keeps what is on disk; a 200 means the server ignored the
    // header and is sending the whole body again.
    let file = if status == StatusCode::PARTIAL_CONTENT {
        seed_hasher(dest, &mut hasher).await?;
        OpenOptions::new().append(true).open(dest).await?
    } else {
        fs::File::create(dest).await?
    };
    let mut writer = BufWriter::with_capacity(256 * 1024, file);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        writer.write_all(&chunk).await?;
        hasher.update(&chunk);

        #[cfg(feature = "events")]
        if let Some(bus) = event_bus {
            bus.emit(Event::Launch(LaunchEvent::InstallProgress {
                bytes: chunk.len() as u64,
            }));
        }
    }

    writer.flush().await?;
    drop(writer);

    // A 200 carrying an HTML error page must not survive as a partial the
    // next mirror would resume on top of.
    if is_zip_like(dest) && !file_has_zip_magic(dest).await {
        let _ = fs::remove_file(dest).await;
        return Err(InstallerError::InvalidArchive {
            url: url.to_string(),
        });
    }

    if let Some(expected) = sha1 {
        let digest = hex::encode(hasher.finalize());

        // Drop the file, or the retry would resume from the corrupt prefix
        // and fail every remaining attempt.
        if !digest.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(dest).await;
            return Err(InstallerError::Sha1Mismatch {
                url: url.to_string(),
                expected: expected.to_string(),
                actual: digest,
            });
        }
    }

    Ok(())
}

fn is_zip_like(dest: &Path) -> bool {
    dest.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("zip"))
}

fn has_zip_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[0] == 0x50 && bytes[1] == 0x4B
}

async fn file_has_zip_magic(dest: &Path) -> bool {
    let mut magic = [0u8; 4];
    match fs::File::open(dest).await {
        Ok(mut file) => file.read_exact(&mut magic).await.is_ok() && has_zip_magic(&magic),
        Err(_) => false,
    }
}

/// Feeds the bytes already on disk into `hasher` so a resumed download still
/// ends up with the digest of the whole file.
async fn seed_hasher(dest: &Path, hasher: &mut Sha1) -> InstallerResult<()> {
    let mut partial = fs::File::open(dest).await?;
    let mut buffer = vec![0u8; 256 * 1024];

    loop {
        let read = partial.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(())
}

/// How a batch moves each file: whole in memory, or streamed to disk with resume.
#[derive(Copy, Clone)]
enum Transfer {
    InMemory,
    Streamed,
}

/// Failures in a row, with no success in between, after which a pass stops
/// starting downloads: the link is down rather than flaky.
const MAX_CONSECUTIVE_FAILURES: usize = 8;

/// Parallelism of the second pass over the files the first one could not get.
const RECOVERY_CONCURRENCY: usize = 4;

/// A task a pass did not complete; `error` is `None` when the pass had
/// already given up by the time its turn came.
struct Unfinished<'t, 'a> {
    task: &'t DownloadTask<'a>,
    error: Option<InstallerError>,
}

/// Runs every task and reports the ones that did not complete, instead of
/// dropping the whole batch on the first failure.
async fn run_pass<'t, 'a>(
    tasks: Vec<&'t DownloadTask<'a>>,
    transfer: Transfer,
    concurrency: usize,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
    #[cfg(feature = "events")] progress: Option<&ProgressState<'_>>,
) -> Vec<Unfinished<'t, 'a>> {
    let semaphore = &Semaphore::new(concurrency.max(1));
    let failure_streak = &AtomicUsize::new(0);

    let futures = tasks.into_iter().map(|task| async move {
        let Ok(_permit) = semaphore.acquire().await else {
            return Some(Unfinished {
                task,
                error: Some(InstallerError::ConcurrencyClosed),
            });
        };

        if failure_streak.load(Ordering::Relaxed) >= MAX_CONSECUTIVE_FAILURES {
            return Some(Unfinished { task, error: None });
        }

        let result = match transfer {
            Transfer::InMemory => {
                download_small_file(
                    task.url,
                    &task.dest,
                    task.sha1,
                    #[cfg(feature = "events")]
                    event_bus,
                )
                .await
            }
            Transfer::Streamed => {
                download_large_file(
                    task.url,
                    &task.dest,
                    task.sha1,
                    #[cfg(feature = "events")]
                    event_bus,
                )
                .await
            }
        };

        match result {
            Ok(()) => {
                failure_streak.store(0, Ordering::Relaxed);

                #[cfg(feature = "events")]
                if let Some(progress) = progress {
                    progress.emit_progress();
                }

                None
            }
            Err(error) => {
                failure_streak.fetch_add(1, Ordering::Relaxed);
                Some(Unfinished {
                    task,
                    error: Some(error),
                })
            }
        }
    });

    join_all(futures).await.into_iter().flatten().collect()
}

async fn download_batch(
    tasks: Vec<DownloadTask<'_>>,
    transfer: Transfer,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
    #[cfg(feature = "events")] progress_kind: Option<DownloadProgressKind>,
) -> InstallerResult<()> {
    let config = get_config();
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

    let mut unfinished = run_pass(
        tasks.iter().collect(),
        transfer,
        config.max_concurrent_downloads,
        #[cfg(feature = "events")]
        event_bus,
        #[cfg(feature = "events")]
        progress.as_ref(),
    )
    .await;

    // A pass that gave up found the link down; a second one would only make
    // the user wait longer for the same error.
    let gave_up = unfinished.iter().any(|entry| entry.error.is_none());

    if !unfinished.is_empty() && !gave_up {
        lighty_core::trace_warn!(
            "[Installer] {} of {} downloads failed, retrying them {} at a time",
            unfinished.len(),
            tasks.len(),
            RECOVERY_CONCURRENCY
        );

        unfinished = run_pass(
            unfinished.into_iter().map(|entry| entry.task).collect(),
            transfer,
            RECOVERY_CONCURRENCY,
            #[cfg(feature = "events")]
            event_bus,
            #[cfg(feature = "events")]
            progress.as_ref(),
        )
        .await;
    }

    match unfinished.into_iter().find_map(|entry| entry.error) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Downloads multiple large files with concurrency limit
pub async fn download_with_concurrency_limit(
    tasks: Vec<DownloadTask<'_>>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
    #[cfg(feature = "events")] progress_kind: Option<DownloadProgressKind>,
) -> InstallerResult<()> {
    download_batch(
        tasks,
        Transfer::Streamed,
        #[cfg(feature = "events")]
        event_bus,
        #[cfg(feature = "events")]
        progress_kind,
    )
    .await
}

/// Downloads multiple small files with concurrency limit
pub async fn download_small_with_concurrency_limit(
    tasks: Vec<DownloadTask<'_>>,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
    #[cfg(feature = "events")] progress_kind: Option<DownloadProgressKind>,
) -> InstallerResult<()> {
    download_batch(
        tasks,
        Transfer::InMemory,
        #[cfg(feature = "events")]
        event_bus,
        #[cfg(feature = "events")]
        progress_kind,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// Serves `/ok/*` always, `/dead/*` never, and `/flaky/*` only from the
    /// request after `failures_before_success` refusals of that same path.
    async fn serve(failures_before_success: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let hits = hits.clone();

                tokio::spawn(async move {
                    let mut request = vec![0u8; 2048];
                    let read = socket.read(&mut request).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&request[..read]).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();

                    let seen = {
                        let mut hits = hits.lock().unwrap();
                        let seen = hits.entry(path.clone()).or_insert(0);
                        *seen += 1;
                        *seen
                    };

                    let served = path.starts_with("/ok")
                        || (path.starts_with("/flaky") && seen > failures_before_success);
                    let response = if served {
                        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody"
                    } else {
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    };

                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        format!("http://{}", address)
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lighty-downloader-{}-{}", name, fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run(tasks: Vec<DownloadTask<'_>>, transfer: Transfer) -> InstallerResult<()> {
        download_batch(
            tasks,
            transfer,
            #[cfg(feature = "events")]
            None,
            #[cfg(feature = "events")]
            None,
        )
        .await
    }

    #[tokio::test]
    async fn one_dead_file_does_not_cancel_the_rest() {
        let base = serve(0).await;
        let dir = scratch_dir("dead");
        let urls = [
            format!("{}/ok/a", base),
            format!("{}/dead/b", base),
            format!("{}/ok/c", base),
        ];
        let tasks = urls
            .iter()
            .zip(["a", "b", "c"])
            .map(|(url, name)| DownloadTask { url, dest: dir.join(name), sha1: None, size: 4 })
            .collect();

        let error = run(tasks, Transfer::InMemory).await.unwrap_err().to_string();

        assert!(error.contains("/dead/b"), "{error}");
        assert!(error.contains("HTTP 500"), "{error}");
        assert_eq!(std::fs::read(dir.join("a")).unwrap(), b"body");
        assert_eq!(std::fs::read(dir.join("c")).unwrap(), b"body");
        assert!(!dir.join("b").exists());
        assert!(!dir.join("a.part").exists());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn second_pass_recovers_what_the_first_gave_up_on() {
        // One more refusal than a single file gets attempts in the first pass.
        let base = serve(get_config().max_retries as usize).await;

        for transfer in [Transfer::InMemory, Transfer::Streamed] {
            let dir = scratch_dir("flaky");
            let name = match transfer {
                Transfer::InMemory => "memory",
                Transfer::Streamed => "streamed",
            };
            let urls = [format!("{}/ok/{}", base, name), format!("{}/flaky/{}", base, name)];
            let tasks = urls
                .iter()
                .zip(["steady", "flaky"])
                .map(|(url, file)| DownloadTask { url, dest: dir.join(file), sha1: None, size: 4 })
                .collect();

            run(tasks, transfer).await.unwrap();

            assert_eq!(std::fs::read(dir.join("flaky")).unwrap(), b"body");
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn a_dead_link_stops_the_batch_early() {
        let base = serve(0).await;
        let dir = scratch_dir("down");
        let urls: Vec<String> = (0..MAX_CONSECUTIVE_FAILURES + 4)
            .map(|index| format!("{}/dead/{}", base, index))
            .collect();
        let tasks: Vec<_> = urls
            .iter()
            .enumerate()
            .map(|(index, url)| DownloadTask { url, dest: dir.join(index.to_string()), sha1: None, size: 4 })
            .collect();
        let refs: Vec<&DownloadTask<'_>> = tasks.iter().collect();

        let unfinished = run_pass(
            refs,
            Transfer::InMemory,
            1,
            #[cfg(feature = "events")]
            None,
            #[cfg(feature = "events")]
            None,
        )
        .await;

        let skipped = unfinished.iter().filter(|entry| entry.error.is_none()).count();
        assert_eq!(unfinished.len(), tasks.len());
        assert_eq!(skipped, 4);

        let _ = std::fs::remove_dir_all(dir);
    }
}
