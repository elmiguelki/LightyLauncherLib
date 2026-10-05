// Copyright (c) 2025 Hamadi
// Licensed under the MIT License

//! Launch and installation events.

use serde::{Deserialize, Serialize};

/// Launch and installation events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum LaunchEvent {
    IsInstalled {
        version: String,
    },
    InstallStarted {
        version: String,
        total_bytes: u64,
    },
    InstallProgress {
        bytes: u64,
    },
    /// Assets download progress (count-based)
    DownloadingAssets {
        current: usize,
        total: usize,
    },
    /// Libraries download progress (count-based)
    DownloadingLibraries {
        current: usize,
        total: usize,
    },
    /// Native libraries download progress (count-based)
    DownloadingNatives {
        current: usize,
        total: usize,
    },
    /// Mods download progress (count-based)
    DownloadingMods {
        current: usize,
        total: usize,
    },
    /// Installation completed
    InstallCompleted {
        version: String,
        total_bytes: u64,
    },
    Launching {
        version: String,
    },
    Launched {
        version: String,
        pid: u32,
    },
    NotLaunched {
        version: String,
        error: String,
    },
    ProcessOutput {
        pid: u32,
        stream: String,
        line: String,
    },
    ProcessExited {
        pid: u32,
        exit_code: i32,
    },
}
