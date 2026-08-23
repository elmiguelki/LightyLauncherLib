use std::env;
use std::time::Duration;
use once_cell::sync::Lazy;
use reqwest::{Client, Url};
use tokio::fs;
use thiserror::Error;

/// Global User-Agent for the launcher (commented placeholder).
///static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Shared HTTP client tuned for parallel asset/library downloads.
///
/// Tuned for the high concurrency of installer workloads: large connection
/// pool, HTTP/2 window scaling, TCP keepalive, and aggressive compression.
pub static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 MiguelkiNetworkLauncher/0.5.11")
        // Connection pooling - balance between performance and OS limits
        .pool_max_idle_per_host(100)
        .pool_idle_timeout(Some(Duration::from_secs(90)))

        // HTTP/2 optimisations
        .http2_initial_stream_window_size(Some(2 * 1024 * 1024))
        .http2_initial_connection_window_size(Some(4 * 1024 * 1024))
        .http2_adaptive_window(true)
        .http2_max_frame_size(Some(16 * 1024))

        // TCP optimisations
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .tcp_nodelay(true)

        // Timeouts - prevent stuck connections
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(5))

        // Compression
        .zstd(true)
        .gzip(true)
        .brotli(true)

        .build()
        .expect("Failed to build HTTP client with default configuration - this should never fail")
});

    /// Fallback client that disables automatic response decompression.
    ///
    /// Some mod-loader or installer endpoints incorrectly advertise a content
    /// encoding that does not match the payload. When that happens, the normal
    /// client can fail with a response-body decode error. This client is used as
    /// a last resort so we can still save the raw bytes when the server returns
    /// an already-plain response or a mislabeled body.
    pub static RAW_HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
        Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 MiguelkiNetworkLauncher/0.5.11")
        .pool_max_idle_per_host(100)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .http2_initial_stream_window_size(Some(2 * 1024 * 1024))
        .http2_initial_connection_window_size(Some(4 * 1024 * 1024))
        .http2_adaptive_window(true)
        .http2_max_frame_size(Some(16 * 1024))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(5))
        .gzip(false)
        .brotli(false)
        .zstd(false)
        .build()
        .expect("Failed to build raw HTTP client - this should never fail")
    });

fn env_base(var: &str) -> Option<String> {
    env::var(var)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

fn join_base_and_path(base: &str, path: &str) -> String {
    if path.starts_with('/') {
        format!("{}{}", base, path)
    } else {
        format!("{}/{}", base, path)
    }
}

fn push_fastmcmirror(urls: &mut Vec<String>, base: &str, path: &str) {
    if !path.is_empty() {
        urls.push(join_base_and_path(base, path));
    }
}

/// Builds a PrismLauncher metadata URL for a package/version pair.
///
/// PrismLauncher stores loader metadata as per-package JSON files under
/// `https://meta.prismlauncher.org/v1/{uid}/{version}.json`.
pub fn prism_meta_url(package_uid: &str, version: &str) -> String {
    format!("https://meta.prismlauncher.org/v1/{}/{}.json", package_uid, version)
}

pub fn build_fallback_urls(original: &str) -> Vec<String> {
    let mut urls = vec![original.to_string()];

    let Ok(parsed) = Url::parse(original) else {
        return urls;
    };

    let host = parsed.host_str().unwrap_or_default();
    let path = parsed.path();

    if host.eq_ignore_ascii_case("resources.download.minecraft.net") && !path.is_empty() {
        push_fastmcmirror(&mut urls, "https://bmclapi2.bangbang93.com/assets", path);
        if let Some(base) = env_base("LIGHTY_MIRROR_MOJANG_ASSETS") {
            urls.push(join_base_and_path(&base, path));
        }
    }

    if host.eq_ignore_ascii_case("piston-meta.mojang.com") && !path.is_empty() {
        push_fastmcmirror(&mut urls, "https://bmclapi2.bangbang93.com", path);
        if let Some(base) = env_base("LIGHTY_MIRROR_PISTON_META") {
            urls.push(join_base_and_path(&base, path));
        }
    }

    if host.eq_ignore_ascii_case("launchermeta.mojang.com") && !path.is_empty() {
        push_fastmcmirror(&mut urls, "https://bmclapi2.bangbang93.com", path);
    }

    if host.eq_ignore_ascii_case("libraries.minecraft.net") && !path.is_empty() {
        push_fastmcmirror(&mut urls, "https://bmclapi2.bangbang93.com/maven", path);
        urls.push(format!("https://repo1.maven.org/maven2{}", path));
        urls.push(format!("https://maven.fabricmc.net{}", path));
    }

    if host.eq_ignore_ascii_case("meta.fabricmc.net") && !path.is_empty() {
        push_fastmcmirror(&mut urls, "https://meta2.fabricmc.net", path);
        // NOTE: bmclapi2.bangbang93.com/fabric-meta returns 404 "未找到fabric meta"
        // Not supported by this mirror, only use if explicitly configured via env var
        if let Some(base) = env_base("LIGHTY_MIRROR_FABRIC_META") {
            urls.push(join_base_and_path(&base, path));
        }
    }

    if host.eq_ignore_ascii_case("maven.fabricmc.net") && !path.is_empty() {
        push_fastmcmirror(&mut urls, "https://bmclapi2.bangbang93.com/maven", path);
        urls.push(format!("https://repo1.maven.org/maven2{}", path));
        if let Some(base) = env_base("LIGHTY_MIRROR_FABRIC_MAVEN") {
            urls.push(join_base_and_path(&base, path));
        }
    }

    if host.eq_ignore_ascii_case("maven.minecraftforge.net") && !path.is_empty() {
        push_fastmcmirror(&mut urls, "https://bmclapi2.bangbang93.com/maven", path);
        urls.push(format!("https://repo1.maven.org/maven2{}", path));
        if let Some(base) = env_base("LIGHTY_MIRROR_FORGE_MAVEN") {
            urls.push(join_base_and_path(&base, path));
        }
    }

    if host.eq_ignore_ascii_case("files.minecraftforge.net") && path.starts_with("/maven") {
        push_fastmcmirror(&mut urls, "https://bmclapi2.bangbang93.com", path);
        urls.push(format!("https://repo1.maven.org/maven2{}", path.trim_start_matches("/maven")));
    }

    if host.eq_ignore_ascii_case("maven.neoforged.net") && !path.is_empty() {
        urls.push(format!("https://repo1.maven.org/maven2{}", path));
        if let Some(base) = env_base("LIGHTY_MIRROR_NEOFORGE_MAVEN") {
            urls.push(join_base_and_path(&base, path));
        }
    }

    if host.eq_ignore_ascii_case("api.adoptium.net") && !path.is_empty() {
        if let Some(base) = env_base("LIGHTY_MIRROR_ADOPTIUM") {
            urls.push(join_base_and_path(&base, path));
        }
    }

    urls
}


/// Path to the system hosts file (OS-dependent).
#[cfg(target_os = "windows")]
const HOSTS_PATH: &str = "System32\\drivers\\etc\\hosts";

#[cfg(not(target_os = "windows"))]
const HOSTS_PATH: &str = "etc/hosts";

/// Auth-critical domains we refuse to see redirected.
///
/// Malware-modified hosts files commonly redirect these to bypass
/// Mojang authentication or impersonate the launcher's update server.
const HOSTS: [&str; 3] = [
    "mojang.com",
    "minecraft.net",
    "lightylauncher.fr",
];

/// Errors related to the hosts file check.
#[derive(Debug, Error)]
pub enum HostsError {
    #[error("Failed to read hosts file at {0}")]
    HostsReadError(String),

    #[error("Hosts file contains blocked entries: {0}")]
    HostsBlocked(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

pub type HostsResult<T> = std::result::Result<T, HostsError>;

/// Checks whether the system hosts file redirects any auth-critical domains.
///
/// Returns `Err(HostsError::HostsBlocked)` if any line in `/etc/hosts`
/// (or the Windows equivalent) maps one of the [`HOSTS`] entries to a
/// non-standard IP. Comment lines are ignored.
pub async fn check_hosts_file() -> HostsResult<()> {
    let hosts_path = if cfg!(target_os = "windows") {
        let system_drive = env::var("SystemDrive").unwrap_or("C:".to_string());
        format!("{}\\{}", system_drive, HOSTS_PATH)
    } else {
        format!("/{}", HOSTS_PATH)
    };

    if !fs::try_exists(&hosts_path).await? {
        return Ok(());
    }

    let hosts_file = fs::read_to_string(&hosts_path)
        .await
        .map_err(|_| HostsError::HostsReadError(hosts_path.clone()))?;

    let flagged_entries: Vec<_> = hosts_file
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| {
            let mut parts = line.split_whitespace();
            let _ip = parts.next();
            parts.filter(|domain| HOSTS.iter().any(|&entry| domain.contains(entry)))
        })
        .map(|s| s.to_string())
        .collect();

    if !flagged_entries.is_empty() {
        return Err(HostsError::HostsBlocked(flagged_entries.join("\n")));
    }

    Ok(())
}