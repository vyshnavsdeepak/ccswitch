use anyhow::{Context, Result};
use std::{fs, path::PathBuf, process::Command};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{
    platform::{detect, Platform},
    sequence::backup_dir,
};

/// Keychain service name for the currently-active token (read by ~/.ccswitchrc).
const ACTIVE_TOKEN_SERVICE: &str = "ccswitch-active-token";

// ── Live credentials (currently active account) ───────────────────────────────

pub fn read_live() -> Result<String> {
    match detect() {
        Platform::MacOS => keychain_read("Claude Code-credentials"),
        Platform::Linux | Platform::Wsl => {
            let path = creds_file_path();
            fs::read_to_string(&path)
                .with_context(|| format!("Cannot read credentials from {}", path.display()))
        }
    }
}

pub fn write_live(credentials: &str) -> Result<()> {
    match detect() {
        Platform::MacOS => keychain_write("Claude Code-credentials", credentials),
        Platform::Linux | Platform::Wsl => {
            let dir = dirs::home_dir().unwrap().join(".claude");
            fs::create_dir_all(&dir)?;
            write_file_600(&dir.join(".credentials.json"), credentials)
        }
    }
}

// ── Per-account backup credentials ───────────────────────────────────────────

pub fn read_backup(num: u32, email: &str) -> Result<String> {
    match detect() {
        Platform::MacOS => keychain_read(&account_service(num, email)),
        Platform::Linux | Platform::Wsl => {
            let path = cred_backup_path(num, email);
            fs::read_to_string(&path)
                .with_context(|| format!("Cannot read backup credentials from {}", path.display()))
        }
    }
}

pub fn write_backup(num: u32, email: &str, credentials: &str) -> Result<()> {
    match detect() {
        Platform::MacOS => keychain_write(&account_service(num, email), credentials),
        Platform::Linux | Platform::Wsl => write_file_600(&cred_backup_path(num, email), credentials),
    }
}

pub fn delete_backup(num: u32, email: &str) -> Result<()> {
    match detect() {
        Platform::MacOS => {
            // Ignore errors — entry may not exist
            let _ = Command::new("security")
                .args(["delete-generic-password", "-s", &account_service(num, email)])
                .output();
            Ok(())
        }
        Platform::Linux | Platform::Wsl => {
            let path = cred_backup_path(num, email);
            if path.exists() {
                fs::remove_file(&path)?;
            }
            Ok(())
        }
    }
}

// ── Active-token slot (read by ~/.ccswitchrc on every new shell) ──────────────

/// Write the currently-active token to the platform secure store.
/// macOS: keychain entry "ccswitch-active-token".
/// Linux/WSL: ~/.claude-switch-backup/active-token (mode 0600).
pub fn write_active_token(token: &str) -> Result<()> {
    match detect() {
        Platform::MacOS => keychain_write(ACTIVE_TOKEN_SERVICE, token),
        Platform::Linux | Platform::Wsl => {
            write_file_600(&active_token_file_path(), token)
        }
    }
}

/// Path to the active-token file used on Linux/WSL.
pub fn active_token_file_path() -> PathBuf {
    backup_dir().join("active-token")
}

/// Path to the shell-sourced rc file managed by ccswitch.
pub fn ccswitchrc_path() -> PathBuf {
    dirs::home_dir()
        .expect("Cannot find home directory")
        .join(".ccswitchrc")
}

/// Write ~/.ccswitchrc if it does not already exist.
/// Returns true if the file was newly created (caller should show setup hint).
pub fn ensure_ccswitchrc() -> Result<bool> {
    let path = ccswitchrc_path();
    if path.exists() {
        return Ok(false);
    }

    let content = match detect() {
        Platform::MacOS => {
            "# Managed by ccswitch — do not edit manually\n\
             export CLAUDE_CODE_OAUTH_TOKEN=$(security find-generic-password \
             -s \"ccswitch-active-token\" -w 2>/dev/null)\n"
        }
        Platform::Linux | Platform::Wsl => {
            "# Managed by ccswitch — do not edit manually\n\
             export CLAUDE_CODE_OAUTH_TOKEN=$(cat \
             ~/.claude-switch-backup/active-token 2>/dev/null)\n"
        }
    };

    fs::write(&path, content)
        .with_context(|| format!("Cannot write {}", path.display()))?;

    Ok(true)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn account_service(num: u32, email: &str) -> String {
    format!("Claude Code-Account-{num}-{email}")
}

fn creds_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap()
        .join(".claude")
        .join(".credentials.json")
}

fn cred_backup_path(num: u32, email: &str) -> PathBuf {
    backup_dir()
        .join("credentials")
        .join(format!(".claude-credentials-{num}-{email}.json"))
}

fn keychain_read(service: &str) -> Result<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .context("Failed to run `security` command")?;

    if !output.status.success() {
        anyhow::bail!("No keychain entry found for service: {service}");
    }

    let mut val = String::from_utf8(output.stdout).context("Keychain returned non-UTF8 data")?;
    // Strip trailing newline added by security(1)
    if val.ends_with('\n') {
        val.pop();
    }
    Ok(val)
}

fn keychain_write(service: &str, value: &str) -> Result<()> {
    let user = std::env::var("USER").unwrap_or_default();
    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            service,
            "-a",
            &user,
            "-w",
            value,
        ])
        .output()
        .context("Failed to run `security` command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to write to keychain: {stderr}");
    }
    Ok(())
}

fn write_file_600(path: &PathBuf, content: &str) -> Result<()> {
    fs::write(path, content)
        .with_context(|| format!("Cannot write to {}", path.display()))?;

    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

    Ok(())
}
