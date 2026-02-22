use anyhow::{Context, Result};
use std::{fs, path::PathBuf, process::Command};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{
    platform::{detect, Platform},
    sequence::backup_dir,
};

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
