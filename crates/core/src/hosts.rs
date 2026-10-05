use std::env;
use std::path::Path;
use std::time::Duration;
use once_cell::sync::Lazy;
use reqwest::{Client, Url};
use thiserror::Error;

/// Shared HTTP client tuned for parallel asset/library downloads.
pub static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("MiguelkiNetworkLauncher/0.5.15 (LightyLauncherLib; BMCLAPI)")
        .pool_max_idle_per_host(100)
        .pool_idle_timeout(Some(Duration::from_secs(90)))

        .http2_initial_stream_window_size(Some(2 * 1024 * 1024))
        .http2_initial_connection_window_size(Some(4 * 1024 * 1024))
        .http2_adaptive_window(true)
        .http2_max_frame_size(Some(16 * 1024))

        .tcp_keepalive(Some(Duration::from_secs(60)))
        .tcp_nodelay(true)

        // Timeouts - prevent stuck connections. The read timeout only
        // fires when no data arrives for 60s; a total timeout would abort
        // any large file (JRE, client jar) on a slow link.
        .read_timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(5))

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
        .user_agent("MiguelkiNetworkLauncher/0.5.15 (LightyLauncherLib; BMCLAPI)")
        .pool_max_idle_per_host(100)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .http2_initial_stream_window_size(Some(2 * 1024 * 1024))
        .http2_initial_connection_window_size(Some(4 * 1024 * 1024))
        .http2_adaptive_window(true)
        .http2_max_frame_size(Some(16 * 1024))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .tcp_nodelay(true)
        .read_timeout(Duration::from_secs(60))
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


#[cfg(target_os = "windows")]
const HOSTS_PATH: &str = "System32\\drivers\\etc\\hosts";

#[cfg(not(target_os = "windows"))]
const HOSTS_PATH: &str = "etc/hosts";

/// Hosts the launcher must be able to reach. Antivirus, parental filters and
/// cracked-launcher installers routinely blackhole these in the hosts file,
/// which breaks login; add a domain here when a launch depends on reaching it.
const HOSTS: [&str; 5] = [
    "mojang.com",
    "minecraft.net",
    "minecraftservices.com",
    "microsoftonline.com",
    "xboxlive.com",
];

/// Errors related to the hosts file check.
#[derive(Debug, Error)]
pub enum HostsError {
    #[error("Failed to read hosts file at {0}")]
    HostsReadError(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

pub type HostsResult<T> = std::result::Result<T, HostsError>;

/// Returns the hosts entries intercepting a domain the launcher needs; empty
/// means the file is clean. `extra` adds a launcher's own auth domain.
///
/// Synchronous on purpose: the file is tiny and [`crate::AppState::init`],
/// which calls it, is not async.
pub fn blocked_launcher_domains(extra: &[&str]) -> HostsResult<Vec<String>> {
    let hosts_path = if cfg!(target_os = "windows") {
        let system_drive = env::var("SystemDrive").unwrap_or("C:".to_string());
        format!("{}\\{}", system_drive, HOSTS_PATH)
    } else {
        format!("/{}", HOSTS_PATH)
    };

    if !Path::new(&hosts_path).exists() {
        return Ok(Vec::new());
    }

    let hosts_file = std::fs::read_to_string(&hosts_path)
        .map_err(|_| HostsError::HostsReadError(hosts_path.clone()))?;

    Ok(hosts_file
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| line.split_whitespace().skip(1))
        .filter(|host| is_watched_host(host, extra))
        .map(str::to_string)
        .collect())
}

/// An entry matches when it is the domain itself or one of its subdomains —
/// a `contains` test also fired on lookalikes such as mojang.com.evil.tld.
fn is_watched_host(host: &str, extra: &[&str]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();

    HOSTS.iter().chain(extra).any(|domain| {
        host == *domain
            || host
                .strip_suffix(*domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}
