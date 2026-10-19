// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! Java process execution wrapper.

use crate::errors::{JavaRuntimeError, JavaRuntimeResult};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};

/// Wrapper around a Java binary path for process execution
pub struct JavaRuntime(pub PathBuf);

impl JavaRuntime {
    /// Creates a new JavaRuntime from a binary path
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    /// Spawns a Java process with the given arguments in `game_dir`.
    pub async fn execute(&self, arguments: Vec<String>, game_dir: &Path) -> JavaRuntimeResult<Child> {
        if !self.0.exists() {
            return Err(JavaRuntimeError::NotFound {
                path: self.0.clone(),
            });
        }

        lighty_core::trace_debug!("Spawning Java process: {:?}", &self.0);

        let mut command = Command::new(&self.0);
        command
            .current_dir(game_dir)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // On Linux, keep the game on X11 (XWayland under Wayland) whenever an X server is
        // reachable: a native Wayland window is invisible to X11-based tools such as window
        // detection or screen capture. Removing WAYLAND_DISPLAY is not enough, because libwayland
        // then falls back to the default `wayland-0` socket; pointing it at a socket that does
        // not exist makes GLFW's Wayland backend fail and fall back to X11.
        #[cfg(target_os = "linux")]
        if std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty()) {
            command.env("WAYLAND_DISPLAY", "lighty-x11-only");
        }

        // On Windows, hide the console window
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let child = command.spawn()?;

        lighty_core::trace_info!("Java process spawned successfully");
        Ok(child)
    }
}
