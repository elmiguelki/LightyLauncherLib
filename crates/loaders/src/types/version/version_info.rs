use std::path::{Path, PathBuf};
use std::time::Duration;

/// Generic view of an installable instance.
///
/// Used by `ManifestRepository` to support different builder types
/// (`VersionBuilder`, `LightyVersionBuilder`, etc.) under a single interface.
pub trait VersionInfo: Clone + Send + Sync {
    type LoaderType: Clone + Send + Sync + std::fmt::Debug;

    /// Instance name (unique profile identifier).
    fn name(&self) -> &str;

    /// Loader version (or server URL for `LightyVersionBuilder`).
    fn loader_version(&self) -> &str;

    /// Minecraft version.
    fn minecraft_version(&self) -> &str;

    /// Instance root directory (holds `runtime/`, `libraries/`,
    /// `assets/`, `.forge/` etc.).
    fn game_dirs(&self) -> &Path;

    /// Java directory (holds JRE installations).
    fn java_dirs(&self) -> &Path;

    /// Root of the files several instances can share: `assets/`,
    /// `libraries/`, client JARs and loader installers. Natives and the
    /// runtime stay under [`Self::game_dirs`].
    ///
    /// Default: alias of [`Self::game_dirs`], which keeps everything
    /// inside the instance.
    fn resources_dir(&self) -> &Path {
        self.game_dirs()
    }

    /// Whether [`Self::resources_dir`] is a store shared across instances.
    fn has_shared_resources(&self) -> bool {
        self.resources_dir() != self.game_dirs()
    }

    /// `assets/` root (indexes + objects).
    fn assets_dir(&self) -> PathBuf {
        self.resources_dir().join("assets")
    }

    /// `libraries/` root (maven layout).
    fn libraries_dir(&self) -> PathBuf {
        self.resources_dir().join("libraries")
    }

    /// Client JAR. Inside the instance it is named after the instance;
    /// in a shared store it is keyed by Minecraft version.
    fn client_jar_path(&self) -> PathBuf {
        if self.has_shared_resources() {
            let mc = self.minecraft_version();
            self.resources_dir()
                .join("versions")
                .join(mc)
                .join(format!("{mc}.jar"))
        } else {
            self.game_dirs().join(format!("{}.jar", self.name()))
        }
    }

    /// Cache directory of a loader installer (`.forge`, `.neoforge`).
    fn loader_cache_dir(&self, dot_dir: &str) -> PathBuf {
        if self.has_shared_resources() {
            self.resources_dir()
                .join("loaders")
                .join(dot_dir.trim_start_matches('.'))
        } else {
            self.game_dirs().join(dot_dir)
        }
    }

    /// Returns the loader.
    fn loader(&self) -> &Self::LoaderType;

    /// Working directory the JVM is launched in — the value passed as
    /// `${game_directory}` to the Minecraft client.
    ///
    /// Default: alias of [`Self::game_dirs`].
    fn runtime_dir(&self) -> &Path {
        self.game_dirs()
    }

    /// Internal setter used by the launch runner to write the
    /// effective runtime dir back onto a mutable builder.
    fn set_runtime_dir(&mut self, _path: PathBuf) {}

    /// Returns whether the game directory exists on disk.
    fn game_dir_exists(&self) -> bool {
        self.game_dirs().exists()
    }

    /// Returns whether the Java directory exists on disk.
    fn java_dir_exists(&self) -> bool {
        self.java_dirs().exists()
    }

    /// Returns a fully qualified version identifier.
    ///
    /// Format: `{name}-{minecraft_version}-{loader_version}`.
    fn full_identifier(&self) -> String {
        format!(
            "{}-{}-{}",
            self.name(),
            self.minecraft_version(),
            self.loader_version()
        )
    }

    /// Returns the (game_dir, java_dir) tuple.
    fn paths(&self) -> (&Path, &Path) {
        (self.game_dirs(), self.java_dirs())
    }

    /// Returns whether the instance is installed (game directory exists).
    fn is_installed(&self) -> bool {
        self.game_dirs().exists()
    }

    /// TTL applied to every cache entry the launcher associates with
    /// this instance. Default = 24h.
    fn ttl(&self) -> Duration {
        Duration::from_secs(86_400)
    }
}
