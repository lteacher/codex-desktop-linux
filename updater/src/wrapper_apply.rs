//! Applies a pending wrapper (repo) update for the current install type.
//!
//! Invoked by the launcher when it consumes the `wrapper-update-pending`
//! marker. Detection (see [`crate::wrapper`]) only records that a newer wrapper
//! build exists; this module performs the actual rebuild + install:
//!
//! - **User-local** installs reuse `~/.local/bin/codex-desktop-update`, which
//!   pulls the managed checkout and re-runs `install.sh` in place as the user
//!   (no privilege escalation).
//! - **Packaged** installs fetch the wrapper source into a managed clone, build
//!   a fresh native package from the cached DMG, and install it with `pkexec`.
//!   When the build toolchain (cargo / node / a DMG extractor) is missing, this
//!   degrades to a desktop notification instead of failing mid-rebuild.

use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use tracing::{info, warn};

use crate::{
    builder,
    config::{RuntimeConfig, RuntimePaths},
    install, notify,
    state::PersistedState,
    upstream,
};

const DEFAULT_WRAPPER_REMOTE: &str = "https://github.com/ilysenko/codex-desktop-linux.git";

/// How the running app was installed, which determines how a wrapper update is
/// applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallType {
    /// Native package under `/opt/codex-desktop` with a system package record.
    Packaged,
    /// `install.sh` install under the user's home (`~/.local/...`).
    UserLocal,
}

fn detect_install_type(config: &RuntimeConfig) -> InstallType {
    if let Some(app_dir) = std::env::var_os("CODEX_LINUX_APP_DIR").map(PathBuf::from) {
        if app_dir.starts_with("/opt/codex-desktop") {
            return InstallType::Packaged;
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            if app_dir.starts_with(home) {
                return InstallType::UserLocal;
            }
        }
    }

    let packaged_bundle = Path::new("/opt/codex-desktop/update-builder");
    if config.builder_bundle_root == packaged_bundle && install::is_primary_package_installed() {
        InstallType::Packaged
    } else {
        InstallType::UserLocal
    }
}

fn set_wrapper_status(state: &mut PersistedState, paths: &RuntimePaths, status: &str) {
    state.wrapper_status = Some(status.to_string());
    let _ = state.save(&paths.state_file);
}

/// Applies a pending wrapper update. No-ops when wrapper tracking is disabled.
pub async fn run_apply_wrapper_update(
    config: &RuntimeConfig,
    state: &mut PersistedState,
    paths: &RuntimePaths,
) -> Result<()> {
    if !config.enable_wrapper_updates {
        println!("Wrapper update tracking is disabled; nothing to apply.");
        return Ok(());
    }

    set_wrapper_status(state, paths, "applying");

    let result = match detect_install_type(config) {
        InstallType::UserLocal => apply_user_local().await,
        InstallType::Packaged => apply_packaged(config, state, paths).await,
    };

    match result {
        Ok(()) => {
            state.wrapper_status = Some("installed".to_string());
            state.clear_wrapper_update_candidate();
            let _ = state.save(&paths.state_file);
            let _ = notify::send(
                "Codex Desktop Linux updated",
                "The newer Linux wrapper build has been installed.",
            );
            Ok(())
        }
        Err(error) => {
            set_wrapper_status(state, paths, "failed");
            warn!(?error, "wrapper update apply failed");
            Err(error)
        }
    }
}

/// User-local apply: reuse the contrib `codex-desktop-update` helper, which
/// pulls the managed wrapper checkout and re-runs `install.sh` in place.
async fn apply_user_local() -> Result<()> {
    let helper = user_local_update_helper().context(
        "user-local wrapper update helper (~/.local/bin/codex-desktop-update) not found",
    )?;
    info!(helper = %helper.display(), "applying wrapper update via user-local helper");
    let status = Command::new(&helper)
        .arg("--quiet")
        .status()
        .with_context(|| format!("Failed to run {}", helper.display()))?;
    if !status.success() {
        anyhow::bail!("{} exited with status {status}", helper.display());
    }
    Ok(())
}

fn user_local_update_helper() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let candidate = home.join(".local/bin/codex-desktop-update");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Packaged apply: fetch fresh wrapper source, rebuild a native package from the
/// cached DMG, and install it with pkexec. Degrades to a notification when the
/// build toolchain is unavailable.
async fn apply_packaged(
    config: &RuntimeConfig,
    state: &mut PersistedState,
    paths: &RuntimePaths,
) -> Result<()> {
    if let Some(missing) = missing_build_dependency() {
        let body = format!(
            "A newer Codex Desktop Linux build is available, but '{missing}' is needed to rebuild it. Install the build tools or update the package manually."
        );
        let _ = notify::send("Codex Desktop Linux update available", &body);
        println!("{body}");
        anyhow::bail!("{body}");
    }

    let wrapper_src = ensure_wrapper_source(config, paths)?;
    let dmg_path = cached_or_downloaded_dmg(config, state, paths).await?;

    // The package version must remain monotonic (timestamp+dmghash), so derive
    // it from the cached DMG the same way the DMG path does.
    let candidate_version = derive_package_version(&dmg_path)?;

    let artifacts = builder::build_update_from(
        &wrapper_src,
        config,
        state,
        paths,
        &candidate_version,
        &dmg_path,
    )
    .await
    .context("wrapper package rebuild failed")?;

    let current_exe = std::env::current_exe().context("Failed to resolve updater binary path")?;
    let output = install::pkexec_command(&current_exe, &artifacts.package_path)
        .output()
        .context("Failed to launch pkexec for wrapper update installation")?;
    if !output.status.success() {
        anyhow::bail!(
            "privileged wrapper install exited with status {}",
            output.status
        );
    }

    state.installed_version = install::installed_package_version();
    let _ = state.save(&paths.state_file);
    Ok(())
}

/// Clones or refreshes a managed wrapper checkout under the workspace cache and
/// returns its path. Never touches the user's working tree.
fn ensure_wrapper_source(config: &RuntimeConfig, paths: &RuntimePaths) -> Result<PathBuf> {
    let remote = resolve_wrapper_remote(config);
    let branch = if config.wrapper_branch.trim().is_empty() {
        "main"
    } else {
        config.wrapper_branch.trim()
    };
    let dest = paths.cache_dir.join("wrapper-src");

    if dest.join(".git").is_dir() {
        run_git(&[
            "-C",
            &dest.to_string_lossy(),
            "fetch",
            "--depth",
            "1",
            "--quiet",
            &remote,
            branch,
        ])?;
        run_git(&[
            "-C",
            &dest.to_string_lossy(),
            "reset",
            "--hard",
            "--quiet",
            "FETCH_HEAD",
        ])?;
        run_git(&["-C", &dest.to_string_lossy(), "clean", "-fdx", "--quiet"])?;
    } else {
        std::fs::create_dir_all(&paths.cache_dir)
            .with_context(|| format!("Failed to create {}", paths.cache_dir.display()))?;
        let _ = std::fs::remove_dir_all(&dest);
        run_git(&[
            "clone",
            "--depth",
            "1",
            "--branch",
            branch,
            "--single-branch",
            "--quiet",
            &remote,
            &dest.to_string_lossy(),
        ])?;
    }

    Ok(dest)
}

fn resolve_wrapper_remote(config: &RuntimeConfig) -> String {
    let trimmed = config.wrapper_remote.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }

    if config.builder_bundle_root.join(".git").is_dir() {
        if let Some(origin) = git_capture(&[
            "-C",
            &config.builder_bundle_root.to_string_lossy(),
            "remote",
            "get-url",
            "origin",
        ]) {
            return origin;
        }
    }

    DEFAULT_WRAPPER_REMOTE.to_string()
}

fn git_capture(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true")
        .env("GCM_INTERACTIVE", "never")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn run_git(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true")
        .env("GCM_INTERACTIVE", "never")
        .status()
        .context("Failed to run git for wrapper source")?;
    if !status.success() {
        anyhow::bail!("git {:?} exited with status {status}", args);
    }
    Ok(())
}

/// Returns the cached DMG path, downloading it if no usable cache exists.
async fn cached_or_downloaded_dmg(
    config: &RuntimeConfig,
    state: &mut PersistedState,
    paths: &RuntimePaths,
) -> Result<PathBuf> {
    if let Some(dmg) = state.artifact_paths.dmg_path.clone() {
        if dmg.exists() {
            return Ok(dmg);
        }
    }

    let client = reqwest::Client::builder().build()?;
    let downloads_dir = config.workspace_root.join("downloads");
    let downloaded =
        upstream::download_dmg(&client, &config.dmg_url, &downloads_dir, chrono::Utc::now())
            .await
            .context("Failed to download upstream DMG for wrapper rebuild")?;
    state.artifact_paths.dmg_path = Some(downloaded.path.clone());
    let _ = state.save(&paths.state_file);
    Ok(downloaded.path)
}

/// Derives a monotonic package version (`YYYY.MM.DD.HHMMSS+<sha8>`) from the DMG
/// contents, matching the DMG update path's scheme.
fn derive_package_version(dmg_path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(dmg_path)
        .with_context(|| format!("Failed to read {}", dmg_path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    upstream::derive_candidate_version(&sha, chrono::Utc::now())
}

/// Returns the first missing build dependency needed for a packaged rebuild, or
/// `None` when the toolchain is present.
fn missing_build_dependency() -> Option<&'static str> {
    // install.sh needs a DMG extractor (7z/7zz) and the package build runs cargo
    // for the updater; node is provided by the bundled managed runtime.
    for (tool, label) in [("cargo", "cargo"), ("7zz", "7zz")] {
        if which(tool).is_none() {
            // 7z is an acceptable alternative to 7zz.
            if tool == "7zz" && which("7z").is_some() {
                continue;
            }
            return Some(label);
        }
    }
    None
}

fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
