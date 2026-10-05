// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! Forge/NeoForge install-processors wrappers.

use std::path::PathBuf;

use lighty_core::download::download_file_untracked;
use lighty_core::mkdir;
use lighty_loaders::types::VersionInfo;
use lighty_core::QueryError;
use lighty_loaders::utils::forge_installer::ForgeInstallProfile;
use lighty_loaders::utils::maven::fetch_maven_sha1;

use super::processor::run_processors;
#[cfg(feature = "forge")]
use super::processor::extract_maven_bundle_to_libraries;

type Result<T> = std::result::Result<T, QueryError>;

/// Path to the per-installer marker file that records the SHA1 of the
/// installer whose processors last ran successfully.
fn processors_marker_path<V: VersionInfo>(version: &V, dot_dir: &str) -> PathBuf {
    let mc = version.minecraft_version();
    let loader_ver = version.loader_version();
    let full_ver = if loader_ver.starts_with(&format!("{}-", mc)) {
        loader_ver.to_string()
    } else {
        format!("{}-{}", mc, loader_ver)
    };
    version.game_dirs().join(dot_dir).join(format!(
        "processors-{}.sha1",
        full_ver
    ))
}

/// Runs the modern Forge install processors (>= 1.13).
///
/// Requires that the install_profile libraries are already on disk.
#[cfg(feature = "forge")]
pub(crate) async fn run_forge_install_processors<V: VersionInfo>(
    version: &V,
    install_profile: &ForgeInstallProfile,
    java_path: PathBuf,
) -> Result<()> {
    use lighty_loaders::forge::forge::{
        build_installer_url, installer_cache_path, FORGE_EXTRACT_SUBDIR, FORGE_MAVEN,
    };

    lighty_core::trace_info!(loader = "forge", "Checking if processors need to run");

    ensure_installer_cached(
        version,
        "forge",
        build_installer_url(version),
        installer_cache_path(version),
    )
        .await?;

    let installer_path = installer_cache_path(version);

    // Some Forge versions ship runtime artifacts at `/maven/...` inside the
    // installer (forge-shim.jar in 1.21+, etc.). Idempotent.
    let libraries_dir = version.game_dirs().join("libraries");
    extract_maven_bundle_to_libraries(&installer_path, &libraries_dir).map_err(|e| {
        QueryError::Conversion {
            message: format!(
                "Failed to extract /maven/ bundle from Forge installer {}: {e}",
                installer_path.display()
            ),
        }
    })?;

    let installer_url = build_installer_url(version);
    let marker_path = processors_marker_path(version, ".forge");
    if let Some(expected_sha1) = fetch_maven_sha1(&installer_url).await {
        if let Ok(existing) = std::fs::read_to_string(&marker_path) {
            if existing.trim() == expected_sha1 {
                lighty_core::trace_info!(
                    loader = "forge",
                    "Processors already executed for this installer, skipping"
                );
                return Ok(());
            }
        }
    }

    run_processors(
        version,
        install_profile,
        installer_path,
        FORGE_MAVEN,
        FORGE_EXTRACT_SUBDIR,
        java_path,
    )
    .await?;

    // Marker write failure must surface: silently swallowing would re-run
    // the heavy processors on the next launch.
    if let Some(expected_sha1) = fetch_maven_sha1(&installer_url).await {
        std::fs::write(&marker_path, expected_sha1).map_err(|e| QueryError::Conversion {
            message: format!(
                "Failed to write Forge processors marker {}: {e}",
                marker_path.display()
            ),
        })?;
    }

    lighty_core::trace_info!(loader = "forge", "Processors completed successfully");
    Ok(())
}

/// Runs the NeoForge install processors.
///
/// Requires that the install_profile libraries are already on disk.
#[cfg(feature = "neoforge")]
pub(crate) async fn run_neoforge_install_processors<V: VersionInfo>(
    version: &V,
    install_profile: &ForgeInstallProfile,
    java_path: PathBuf,
) -> Result<()> {
    use lighty_loaders::neoforge::neoforge::{
        build_installer_url, installer_cache_path, NEOFORGE_EXTRACT_SUBDIR, NEOFORGE_MAVEN,
    };

    lighty_core::trace_info!(loader = "neoforge", "Checking if processors need to run");

    ensure_installer_cached(
        version,
        "neoforge",
        build_installer_url(version),
        installer_cache_path(version),
    )
        .await?;

    let installer_path = installer_cache_path(version);

    let installer_url = build_installer_url(version);
    let marker_path = processors_marker_path(version, ".neoforge");
    if let Some(expected_sha1) = fetch_maven_sha1(&installer_url).await {
        if let Ok(existing) = std::fs::read_to_string(&marker_path) {
            if existing.trim() == expected_sha1 {
                lighty_core::trace_info!(
                    loader = "neoforge",
                    "Processors already executed for this installer, skipping"
                );
                return Ok(());
            }
        }
    }

    run_processors(
        version,
        install_profile,
        installer_path,
        NEOFORGE_MAVEN,
        NEOFORGE_EXTRACT_SUBDIR,
        java_path,
    )
    .await?;

    // Same as Forge: marker failure must surface, not be lost.
    if let Some(expected_sha1) = fetch_maven_sha1(&installer_url).await {
        std::fs::write(&marker_path, expected_sha1).map_err(|e| QueryError::Conversion {
            message: format!(
                "Failed to write NeoForge processors marker {}: {e}",
                marker_path.display()
            ),
        })?;
    }

    lighty_core::trace_info!(loader = "neoforge", "Processors completed successfully");
    Ok(())
}

async fn ensure_installer_cached<V: VersionInfo>(
    _version: &V,
    loader_name: &str,
    installer_url: String,
    installer_path: PathBuf,
) -> Result<()> {
    if installer_path.exists() {
        return Ok(());
    }

    if let Some(parent) = installer_path.parent() {
        mkdir!(parent);
    }

    lighty_core::trace_warn!(
        path = ?installer_path,
        loader = loader_name,
        "Installer JAR missing before processor phase; re-downloading"
    );

    download_file_untracked(&installer_url, &installer_path)
        .await
        .map_err(|e| QueryError::Conversion {
            message: format!(
                "Installer JAR missing and could not be restored automatically: {}",
                e
            ),
        })?;

    let expected_sha1 = fetch_maven_sha1(&installer_url)
        .await
        .ok_or_else(|| QueryError::Conversion {
            message: "Failed to fetch SHA1 for installer JAR".to_string(),
        })?;

    lighty_core::verify_file_sha1_sync(&installer_path, &expected_sha1).map_err(|e| {
        QueryError::Conversion {
            message: format!("Downloaded installer has invalid SHA1: {}", e),
        }
    })?;

    lighty_core::trace_info!(
        path = ?installer_path,
        loader = loader_name,
        "Restored missing installer JAR successfully"
    );

    Ok(())
}
