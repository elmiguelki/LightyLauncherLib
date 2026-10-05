use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use lighty_auth::UserProfile;
use lighty_core::time_it;
#[cfg(feature = "events")]
use lighty_event::EventBus;
use lighty_java::jre_downloader::{find_java_binary, jre_download};
use lighty_java::runtime::JavaRuntime;
use lighty_java::JavaDistribution;
use lighty_loaders::types::version_metadata::{Version, VersionMetaData};
use lighty_loaders::types::{Loader, LoaderExtensions, VersionInfo};
use lighty_modsloader::WithMods;

use crate::arguments::Arguments;
use crate::errors::{InstallerError, InstallerResult};
use crate::installer::Installer;

use lighty_core::hosts::{build_fallback_urls, HTTP_CLIENT as CLIENT};
use lighty_core::verify_file_sha1;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

#[cfg(any(feature = "neoforge", feature = "forge"))]
use crate::installer::ressources::libraries::{collect_library_tasks, download_libraries};

use super::builder::LaunchBuilder;

#[cfg(feature = "forge")]
use crate::installer::processors::forge_install::run_forge_install_processors;
#[cfg(feature = "neoforge")]
use crate::installer::processors::forge_install::run_neoforge_install_processors;

#[cfg(feature = "forge")]
use lighty_loaders::forge::forge::{
    extract_install_profile_libraries_modern as forge_install_profile_libraries_modern,
    ForgeRawData, FORGE,
};
#[cfg(feature = "forge")]
use lighty_loaders::forge::forge_legacy::extract_universal_jar as forge_legacy_extract_universal_jar;
#[cfg(feature = "neoforge")]
use lighty_loaders::neoforge::neoforge::{
    extract_install_profile_libraries as neoforge_install_profile_libraries, NEOFORGE,
};

/// Extension trait that adds [`Self::launch`] to any installable instance.
///
/// Implemented automatically for every type that satisfies the launch
/// pipeline's trait bounds (see the blanket impl below).
pub trait Launch {
    /// Launch the game with a builder pattern
    ///
    /// # Arguments
    /// - `profile`: User profile from authentication
    /// - `java_distribution`: Java distribution to use
    ///
    /// # Returns
    /// A `LaunchBuilder` for configuring JVM options and game arguments
    ///
    /// # Example
    /// ```no_run
    /// // Simple launch
    /// version.launch(&profile, JavaDistribution::Zulu).await?;
    ///
    /// // With custom options
    /// version.launch(&profile, JavaDistribution::Zulu)
    ///     .with_jvm_options()
    ///         .set("Xmx", "4G")
    ///         .done()
    ///     .with_arguments()
    ///         .set(KEY_WIDTH, "1920")
    ///         .done()
    ///     .await?;
    /// ```
    fn launch<'a>(
        &'a mut self,
        profile: &'a UserProfile,
        java_distribution: JavaDistribution,
    ) -> LaunchBuilder<'a, Self>
    where
        Self: Sized;
}

// Blanket impl for any type that implements the launch pipeline traits.
//
// `WithMods` is always required (provides `mod_requests()` — defaults to
// `&[]` for types that don't track mods, so it's free for vanilla
// instances). Modrinth / CurseForge / modpack features only gate which
// builder methods exist and which API clients compile in.
impl<T> Launch for T
where
    T: VersionInfo<LoaderType = Loader>
        + LoaderExtensions
        + Arguments
        + Installer
        + WithMods,
{
    fn launch<'a>(
        &'a mut self,
        profile: &'a UserProfile,
        java_distribution: JavaDistribution,
    ) -> LaunchBuilder<'a, Self> {
        LaunchBuilder::new(self, profile, java_distribution)
    }
}

/// Internal function to execute the launch process
pub(crate) async fn execute_launch<T>(
    version: &mut T,
    profile: &UserProfile,
    java_distribution: JavaDistribution,
    jvm_overrides: &std::collections::HashMap<String, String>,
    jvm_removals: &std::collections::HashSet<String>,
    arg_overrides: &std::collections::HashMap<String, String>,
    arg_removals: &std::collections::HashSet<String>,
    raw_args: &[String],
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()>
where
    T: VersionInfo<LoaderType = Loader>
        + LoaderExtensions
        + Arguments
        + Installer
        + WithMods,
{
    // 1. Fetch the loader metadata
    let metadata = prepare_metadata(
        version,
        #[cfg(feature = "events")]
        event_bus,
    )
    .await?;

    let version_data = extract_version(&metadata)?;

    // 2. Make sure Java is installed
    let java_path = ensure_java_installed(
        version,
        version_data,
        &java_distribution,
        #[cfg(feature = "events")]
        event_bus,
    )
    .await?;

    // Reconcile arg_overrides[KEY_GAME_DIRECTORY] back onto the
    // builder so install + args read the same value via
    // version.runtime_dir(). `game_dirs.join(custom)` does what we
    // want: a relative override ("runtime") resolves to
    // game_dirs/runtime, an absolute override ("/mnt/games") wins
    // outright (Path::join semantics).
    if let Some(custom) = arg_overrides.get(crate::arguments::KEY_GAME_DIRECTORY) {
        let resolved = version.game_dirs().join(custom);
        if resolved.as_path() != version.runtime_dir() {
            lighty_core::trace_info!(
                from = %version.runtime_dir().display(),
                to = %resolved.display(),
                source = %custom,
                "[Launch] Resolved KEY_GAME_DIRECTORY override before install"
            );
            version.set_runtime_dir(resolved);
        }
    }

    // Modpack + user-attached mods are resolved by `Installer::install`
    // itself (Phase 0 of its pipeline). The runner just passes the raw
    // `Version` here — no merge to do.

    // 3. Install Minecraft dependencies (libraries, natives, client, assets)
    // Before install, ensure the asset index exists on disk (with fallbacks).
    ensure_asset_index_exists(version, version_data).await?;
    time_it!(
        "Install delay",
        version
            .install(
                version_data,
                #[cfg(feature = "events")]
                event_bus,
            )
            .await?
    );

    // 3b. Forge-family install_profile libraries + processors.
    //
    // For Forge and NeoForge, the install_profile.json libraries are
    // downloaded through the shared library installer (parallel +
    // retry + SHA1) so the processor JARs and the runtime-required
    // `forge:universal` artifact land on disk. Only the processor
    // execution stays inside each loader crate (it's a per-loader
    // Java exec with different maven URLs / extract subdirs).
    //
    // A LightyUpdater builder carries a server URL where the loader version
    // belongs and no Minecraft version, so the installer paths and cache keys
    // below have to be built from the resolved coordinates, not from it.
    #[cfg(any(feature = "forge", feature = "neoforge"))]
    let resolved = version.resolved_instance().await?;

    #[cfg(feature = "neoforge")]
    if matches!(resolved.loader(), Loader::NeoForge) {
        let install_profile = NEOFORGE.get_raw(&resolved).await?;
        let profile_libs = neoforge_install_profile_libraries(install_profile.as_ref());
        let profile_tasks = collect_library_tasks(&resolved, &profile_libs).await;
        download_libraries(
            profile_tasks,
            #[cfg(feature = "events")]
            event_bus,
        )
        .await?;
        run_neoforge_install_processors(&resolved, install_profile.as_ref(), java_path.clone())
            .await?;
    }

    #[cfg(feature = "forge")]
    if matches!(resolved.loader(), Loader::Forge) {
        let raw = FORGE.get_raw(&resolved).await?;
        match raw.as_ref() {
            ForgeRawData::Modern {
                install_profile, ..
            } => {
                // Download processor-only libraries, then run processors.
                let profile_libs = forge_install_profile_libraries_modern(install_profile);
                let profile_tasks = collect_library_tasks(&resolved, &profile_libs).await;
                download_libraries(
                    profile_tasks,
                    #[cfg(feature = "events")]
                    event_bus,
                )
                .await?;
                run_forge_install_processors(&resolved, install_profile, java_path.clone()).await?;
            }
            ForgeRawData::Legacy(profile) => {
                // No processors in the legacy era; the universal JAR
                // ships inside the installer and must be extracted to
                // its Maven path so the classpath entry resolves.
                forge_legacy_extract_universal_jar(&resolved, profile).await?;
            }
        }
    }

    // Launch the game
    execute_game(
        version,
        version_data,
        profile,
        java_path,
        arg_overrides,
        arg_removals,
        jvm_overrides,
        jvm_removals,
        raw_args,
        #[cfg(feature = "events")]
        event_bus,
    )
    .await
}

/// Fetches the loader's full metadata document.
async fn prepare_metadata<T>(
    builder: &mut T,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<Arc<VersionMetaData>>
where
    T: VersionInfo<LoaderType = Loader> + LoaderExtensions,
{
    lighty_core::trace_debug!(
        "[Launch] Fetching metadata for loader: {:?}",
        builder.loader()
    );

    #[cfg(feature = "events")]
    let loader_name = format!("{:?}", builder.loader());

    #[cfg(feature = "events")]
    if let Some(bus) = event_bus {
        bus.emit(lighty_event::Event::Loader(
            lighty_event::LoaderEvent::FetchingData {
                loader: loader_name.clone(),
                minecraft_version: builder.minecraft_version().to_string(),
                loader_version: builder.loader_version().to_string(),
            },
        ));
    }

    // Generic metadata fetching - automatically dispatches to the correct loader
    let metadata = builder.get_metadata().await?;

    #[cfg(feature = "events")]
    if let Some(bus) = event_bus {
        bus.emit(lighty_event::Event::Loader(
            lighty_event::LoaderEvent::DataFetched {
                loader: loader_name,
                minecraft_version: builder.minecraft_version().to_string(),
                loader_version: builder.loader_version().to_string(),
            },
        ));
    }

    lighty_core::trace_info!(
        "[Launch] Metadata fetched successfully for {:?}",
        builder.loader()
    );
    Ok(metadata)
}

/// Ensures Java is installed for `version` and returns the binary path.
async fn ensure_java_installed<T>(
    builder: &T,
    version: &Version,
    java_distribution: &JavaDistribution,
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<PathBuf>
where
    T: VersionInfo,
{
    let java_version = version.java_version.major_version;

    // Look for an existing Java install before downloading
    match find_java_binary(builder.java_dirs(), java_distribution, &java_version).await {
        Ok(path) => {
            lighty_core::trace_info!(
                "[Java] Java {} already installed at: {:?}",
                java_version,
                path
            );
            // Validate the binary; if valid, emit event and return it. If not,
            // remove the runtime directory and proceed to download a fresh copy.
            let is_valid = validate_java_binary(&path).await;

            if is_valid {
                #[cfg(feature = "events")]
                if let Some(bus) = event_bus {
                    bus.emit(lighty_event::Event::Java(lighty_event::JavaEvent::JavaAlreadyInstalled {
                        distribution: java_distribution.get_name().to_string(),
                        version: java_version,
                        binary_path: path.to_string_lossy().to_string(),
                    }));
                }

                return Ok(path);
            }

            lighty_core::trace_warn!("[Java] Existing Java at {:?} failed validation; removing and re-downloading", path);
            let runtime_dir = builder.java_dirs().join(format!("{}_{}", java_distribution.get_name(), java_version));
            if let Err(e) = tokio::fs::remove_dir_all(&runtime_dir).await {
                lighty_core::trace_error!("[Java] Failed to remove invalid runtime dir {:?}: {}", runtime_dir, e);
            }

            // Fallthrough to download logic below
        }
        Err(_) => {
            lighty_core::trace_info!("[Java] Java {} not found, downloading...", java_version);

            #[cfg(feature = "events")]
            if let Some(bus) = event_bus {
                bus.emit(lighty_event::Event::Java(
                    lighty_event::JavaEvent::JavaNotFound {
                        distribution: java_distribution.get_name().to_string(),
                        version: java_version,
                    },
                ));
            }
        }
    }

    // Download JRE (either not found or invalid existing install)
    #[cfg(feature = "events")]
    let path = jre_download(
        builder.java_dirs(),
        java_distribution,
        &java_version,
        |current, total| {
            lighty_core::trace_debug!("[Java] Download progress: {}/{}", current, total);
        },
        event_bus,
    ).await?;

    #[cfg(not(feature = "events"))]
    let path = jre_download(
        builder.java_dirs(),
        java_distribution,
        &java_version,
        |current, total| {
            lighty_core::trace_debug!("[Java] Download progress: {}/{}", current, total);
        },
    ).await?;

    lighty_core::trace_info!("[Java] Java {} installed successfully", java_version);
    Ok(path)
}

/// Validates a Java binary by running `<bin> -version` with a short timeout.
async fn validate_java_binary(path: &std::path::Path) -> bool {
    // Short timeout to avoid blocking the launcher
    const TIMEOUT_MS: u64 = 3000;

    // Try to execute the binary with `-version` and consider it valid when the
    // process exits successfully and emits some output (stdout or stderr).
    // Build a platform-aware command to avoid flashing a console on Windows
    #[cfg(windows)]
    let std_cmd = {
        use std::process::Command as StdCommand;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        let mut c = StdCommand::new(path);
        c.arg("-version");
        c.creation_flags(CREATE_NO_WINDOW);
        c
    };

    #[cfg(not(windows))]
    let std_cmd = {
        use std::process::Command as StdCommand;
        let mut c = StdCommand::new(path);
        c.arg("-version");
        c
    };

    let cmd = Command::from(std_cmd).output();

    match timeout(Duration::from_millis(TIMEOUT_MS), cmd).await {
        Ok(Ok(output)) => {
            if output.status.success() {
                // Some Java distributions print to stderr for `-version`.
                let has_output = !output.stdout.is_empty() || !output.stderr.is_empty();
                return has_output;
            }
            false
        }
        _ => false,
    }
}

/// Spawns the game process and wires up event/console handlers.
async fn execute_game<T>(
    builder: &T,
    version: &Version,
    profile: &UserProfile,
    java_path: PathBuf,
    arg_overrides: &HashMap<String, String>,
    arg_removals: &HashSet<String>,
    jvm_overrides: &HashMap<String, String>,
    jvm_removals: &HashSet<String>,
    raw_args: &[String],
    #[cfg(feature = "events")] event_bus: Option<&EventBus>,
) -> InstallerResult<()>
where
    T: VersionInfo + Arguments,
{
    use crate::instance::manager::GameInstance;
    use crate::instance::{handle_console_streams, INSTANCE_MANAGER};

    let username = profile.username.as_str();

    // Build the full argv (JVM args + main class + game args)
    let arguments = builder.build_arguments(
        version,
        Some(profile),
        arg_overrides,
        arg_removals,
        jvm_overrides,
        jvm_removals,
        raw_args,
    );

    // Launch from the effective runtime directory: the runner already
    // reconciled arg_overrides[KEY_GAME_DIRECTORY] onto runtime_dir().
    let runtime_dir = builder.runtime_dir().to_path_buf();

    if !runtime_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&runtime_dir) {
            lighty_core::trace_warn!("[Launch] Failed to create runtime_dir {:?}: {}", runtime_dir, e);
        }
    }

    // Wrap the Java binary path in a runtime helper
    let java_runtime = JavaRuntime::new(java_path);
    lighty_core::trace_info!("[Launch] Executing game in runtime_dir {:?}...", runtime_dir);

    match java_runtime.execute(arguments, &runtime_dir).await {
        Ok(child) => {
            let pid = child.id().ok_or(InstallerError::NoPid)?;

            lighty_core::trace_info!("[Launch] Game launched successfully, PID: {}", pid);

            // Register the instance (metadata only — the child is owned by the console task)
            let instance = GameInstance {
                pid,
                instance_name: builder.name().to_string(),
                version: format!(
                    "{}-{}",
                    builder.minecraft_version(),
                    builder.loader_version()
                ),
                username: username.to_string(),
                // Conserver game_dirs() comme racine logique de l'instance
                // (données persistantes), même si le runtime effectif peut
                // pointer vers un répertoire éphémère.
                game_dir: builder.game_dirs().to_path_buf(),
                started_at: std::time::SystemTime::now(),
            };

            if let Err(e) = INSTANCE_MANAGER.register_instance(instance).await {
                lighty_core::trace_warn!(
                    error = %e,
                    "Failed to register launched instance — process keeps running"
                );
            }

            // Emit InstanceLaunched event
            #[cfg(feature = "events")]
            if let Some(bus) = event_bus {
                use lighty_event::{Event, InstanceLaunchedEvent};

                bus.emit(Event::InstanceLaunched(InstanceLaunchedEvent {
                    pid,
                    instance_name: builder.name().to_string(),
                    version: format!(
                        "{}-{}",
                        builder.minecraft_version(),
                        builder.loader_version()
                    ),
                    username: username.to_string(),
                    timestamp: std::time::SystemTime::now(),
                }));

                // Spawn the window-appearance watcher
                let bus_clone = bus.clone();
                let instance_name = builder.name().to_string();
                let version = format!(
                    "{}-{}",
                    builder.minecraft_version(),
                    builder.loader_version()
                );
                tokio::spawn(super::window::detect_window_appearance(
                    pid,
                    instance_name,
                    version,
                    bus_clone,
                ));
            }

            // Spawn the console-streaming handler. It takes ownership of the
            // child and handles all stdio until the process exits.
            tokio::spawn(handle_console_streams(
                pid,
                builder.name().to_string(),
                child,
                #[cfg(feature = "events")]
                event_bus.cloned(),
            ));

            Ok(())
        }
        Err(e) => {
            lighty_core::trace_error!("[Launch] Failed to launch game: {}", e);
            Err(e.into())
        }
    }
}

/// Extracts the [`Version`] payload from a [`VersionMetaData`] variant.
fn extract_version(metadata: &VersionMetaData) -> InstallerResult<&Version> {
    match metadata {
        VersionMetaData::Version(v) => Ok(v),
        _ => Err(InstallerError::InvalidMetadata),
    }
}

/// Ensure the asset index JSON exists on disk and is valid. Downloads it if missing or invalid.
async fn ensure_asset_index_exists<T>(
    builder: &T,
    version: &Version,
) -> InstallerResult<()>
where
    T: VersionInfo,
{
    if let Some(asset_index) = &version.assets_index {
        let indexes_dir = builder.game_dirs().join("assets").join("indexes");
        // Ensure directory exists
        lighty_core::mkdir!(indexes_dir);

        let index_file_path = builder
            .game_dirs()
            .join("assets")
            .join("indexes")
            .join(format!("{}.json", asset_index.id));

        // If exists and valid, nothing to do
        if index_file_path.exists() {
            match verify_file_sha1(&index_file_path, &asset_index.sha1).await {
                Ok(true) => return Ok(()),
                _ => {
                    let _ = tokio::fs::remove_file(&index_file_path).await;
                    lighty_core::trace_warn!(
                        "[Assets] Asset index {} missing or invalid on disk, will re-download",
                        asset_index.id
                    );
                }
            }
        }

        // Try download from fallback URLs
        for candidate in build_fallback_urls(&asset_index.url) {
            let resp = CLIENT.get(&candidate).send().await?;
            if !resp.status().is_success() {
                continue;
            }

            let bytes = resp.bytes().await?;
            tokio::fs::write(&index_file_path, &bytes).await?;

            match verify_file_sha1(&index_file_path, &asset_index.sha1).await {
                Ok(true) => {
                    lighty_core::trace_info!(
                        "[Assets] Asset index {} downloaded and verified",
                        asset_index.id
                    );
                    return Ok(());
                }
                _ => {
                    let _ = tokio::fs::remove_file(&index_file_path).await;
                    continue;
                }
            }
        }

        return Err(InstallerError::DownloadFailed(format!(
            "Failed to download or verify asset index {}",
            asset_index.id
        )));
    }

    Ok(())
}
