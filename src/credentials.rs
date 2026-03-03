use anyhow::{Context, Result};
use std::{fs, path::PathBuf, process::Command};

const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

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
            let path = creds_file_path();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_file_600(&path, credentials)
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

// ── Active-token slot (kept for verification / backwards compat) ──────────────

/// Write the currently-active token to the platform secure store.
/// macOS: keychain entry "ccswitch-active-token".
/// Linux/WSL: ~/.claude-switch-backup/active-token (mode 0600).
/// This is no longer the primary auth mechanism — it's kept so that
/// `security find-generic-password -s ccswitch-active-token -w` still works
/// as a quick verification command.
pub fn write_active_token(token: &str) -> Result<()> {
    match detect() {
        Platform::MacOS => keychain_write(ACTIVE_TOKEN_SERVICE, token),
        Platform::Linux | Platform::Wsl => write_file_600(&active_token_file_path(), token),
    }
}

/// Write a plain access token directly to the live credentials store,
/// formatted as OAuth credentials so Claude Code reads it from the keychain
/// without requiring `CLAUDE_CODE_OAUTH_TOKEN` to be set.
///
/// `expiresAt` is set ~10 years out to prevent Claude Code from attempting a
/// refresh (there is no refresh token for static token accounts).
pub fn write_live_token(token: &str) -> Result<()> {
    let expires_at_ms =
        chrono::Utc::now().timestamp_millis() + 10 * 365 * 24 * 3600 * 1000_i64;
    let creds = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": token,
            "refreshToken": "",
            "expiresAt": expires_at_ms,
            "scopes": [
                "user:inference",
                "user:profile",
                "user:sessions:claude_code",
                "user:mcp_servers"
            ]
        }
    })
    .to_string();
    write_live(&creds)
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

/// ccswitch now manages all accounts via the system credentials keychain.
/// The rc file only needs to unset CLAUDE_CODE_OAUTH_TOKEN so Claude Code
/// reads from the keychain instead of an env var override.
fn ccswitchrc_content() -> &'static str {
    concat!(
        "# Managed by ccswitch — do not edit manually\n",
        "# ccswitch writes all credentials directly to the system keychain.\n",
        "# Unsetting CLAUDE_CODE_OAUTH_TOKEN ensures Claude Code reads from\n",
        "# the keychain so account switches take effect on next restart.\n",
        "unset CLAUDE_CODE_OAUTH_TOKEN\n",
    )
}

/// Write ~/.ccswitchrc if it does not exist, or upgrade it if it is outdated.
/// Returns true only when the file is newly created (caller may show a hint).
pub fn ensure_ccswitchrc() -> Result<bool> {
    let path = ccswitchrc_path();
    let content = ccswitchrc_content();

    if !path.exists() {
        fs::write(&path, content)
            .with_context(|| format!("Cannot write {}", path.display()))?;
        return Ok(true);
    }

    let existing = fs::read_to_string(&path)
        .with_context(|| format!("Cannot read {}", path.display()))?;

    // Upgrade if still using the old env-var export approach.
    if existing.contains("export CLAUDE_CODE_OAUTH_TOKEN")
        || !existing.contains("unset CLAUDE_CODE_OAUTH_TOKEN")
    {
        fs::write(&path, content)
            .with_context(|| format!("Cannot write {}", path.display()))?;
    }

    Ok(false)
}

// ── OAuth session status & refresh ───────────────────────────────────────────

/// Extract the `expiresAt` timestamp (milliseconds since Unix epoch) from an
/// OAuth credentials JSON blob.  Returns `None` for token accounts or if the
/// field is absent.
pub fn oauth_expires_at(creds_json: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(creds_json).ok()?;
    v.get("claudeAiOauth")?.get("expiresAt")?.as_i64()
}

/// Returns `true` if the OAuth session is still active (not yet expired).
/// Returns `true` for non-OAuth credentials (no expiry information available).
pub fn is_oauth_active(creds_json: &str) -> bool {
    match oauth_expires_at(creds_json) {
        None => true,
        Some(expires_at_ms) => expires_at_ms > chrono::Utc::now().timestamp_millis(),
    }
}

/// Returns how many whole seconds remain until expiry, or `None` for non-OAuth
/// credentials.  A negative value means the token has already expired.
pub fn oauth_secs_remaining(creds_json: &str) -> Option<i64> {
    let expires_at_ms = oauth_expires_at(creds_json)?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    Some((expires_at_ms - now_ms) / 1000)
}

/// Refresh an OAuth credentials blob using the embedded refresh token.
/// Returns the updated JSON string that should be written back to storage.
pub fn refresh_oauth_creds(creds_json: &str) -> Result<String> {
    let mut v: serde_json::Value =
        serde_json::from_str(creds_json).context("Invalid credentials JSON")?;

    let refresh_token = v
        .get("claudeAiOauth")
        .context("Not an OAuth credentials file (missing claudeAiOauth)")?
        .get("refreshToken")
        .and_then(|t| t.as_str())
        .context("No refreshToken found in credentials")?
        .to_string();

    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": OAUTH_CLIENT_ID,
    });

    let resp = ureq::post(OAUTH_TOKEN_URL)
        .set("Content-Type", "application/json")
        .set("anthropic-beta", OAUTH_BETA_HEADER)
        .send_json(body);

    let resp_json: serde_json::Value = match resp {
        Ok(r) => r
            .into_json::<serde_json::Value>()
            .context("Failed to parse token refresh response")?,
        Err(ureq::Error::Status(code, r)) => {
            let err = r
                .into_json::<serde_json::Value>()
                .unwrap_or(serde_json::Value::Null);
            let desc = err["error_description"]
                .as_str()
                .or_else(|| err["error"].as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Token refresh failed (HTTP {}): {}", code, desc);
        }
        Err(e) => anyhow::bail!("Token refresh request failed: {}", e),
    };

    let new_access = resp_json
        .get("access_token")
        .or_else(|| resp_json.get("accessToken"))
        .and_then(serde_json::Value::as_str)
        .context("No access_token in refresh response")?;

    let new_refresh = resp_json
        .get("refresh_token")
        .or_else(|| resp_json.get("refreshToken"))
        .and_then(serde_json::Value::as_str)
        .context("No refresh_token in refresh response")?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let new_expires_at = if let Some(ea) = resp_json
        .get("expiresAt")
        .or_else(|| resp_json.get("expires_at"))
        .and_then(serde_json::Value::as_i64)
    {
        ea
    } else if let Some(ei) = resp_json
        .get("expires_in")
        .or_else(|| resp_json.get("expiresIn"))
        .and_then(serde_json::Value::as_i64)
    {
        now_ms + ei * 1000
    } else {
        now_ms + 30 * 24 * 3600 * 1000 // fallback: 30 days
    };

    v["claudeAiOauth"]["accessToken"] =
        serde_json::Value::String(new_access.to_string());
    v["claudeAiOauth"]["refreshToken"] =
        serde_json::Value::String(new_refresh.to_string());
    v["claudeAiOauth"]["expiresAt"] =
        serde_json::Value::Number(new_expires_at.into());

    serde_json::to_string(&v).context("Failed to serialize updated credentials")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn account_service(num: u32, email: &str) -> String {
    format!("Claude Code-Account-{num}-{email}")
}

fn creds_file_path() -> PathBuf {
    #[cfg(test)]
    if let Ok(dir) = std::env::var("CCSWITCH_TEST_DIR") {
        return PathBuf::from(dir).join(".credentials.json");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_oauth_creds(expires_at_ms: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-test",
                "refreshToken": "sk-ant-ort01-test",
                "expiresAt": expires_at_ms
            }
        })
        .to_string()
    }

    #[test]
    fn test_oauth_expires_at() {
        let creds = make_oauth_creds(9_999_999_999_999);
        assert_eq!(oauth_expires_at(&creds), Some(9_999_999_999_999));
    }

    #[test]
    fn test_oauth_expires_at_missing() {
        let creds = r#"{"claudeAiOauth": {"accessToken": "tok"}}"#;
        assert_eq!(oauth_expires_at(creds), None);
    }

    #[test]
    fn test_oauth_expires_at_non_oauth() {
        let creds = r#"{"token": "sk-ant-oat01-abc"}"#;
        assert_eq!(oauth_expires_at(creds), None);
    }

    #[test]
    fn test_is_oauth_active_future() {
        let ms = chrono::Utc::now().timestamp_millis() + 3_600_000;
        assert!(is_oauth_active(&make_oauth_creds(ms)));
    }

    #[test]
    fn test_is_oauth_active_past() {
        let ms = chrono::Utc::now().timestamp_millis() - 1_000;
        assert!(!is_oauth_active(&make_oauth_creds(ms)));
    }

    #[test]
    fn test_is_oauth_active_no_expiry() {
        // No expiresAt field → treated as active
        let creds = r#"{"claudeAiOauth": {"accessToken": "tok"}}"#;
        assert!(is_oauth_active(creds));
    }

    #[test]
    fn test_oauth_secs_remaining_positive() {
        let ms = chrono::Utc::now().timestamp_millis() + 3_600_000; // +1h
        let secs = oauth_secs_remaining(&make_oauth_creds(ms)).unwrap();
        assert!(secs > 3500 && secs <= 3600, "expected ~3600, got {secs}");
    }

    #[test]
    fn test_oauth_secs_remaining_negative() {
        let ms = chrono::Utc::now().timestamp_millis() - 3_600_000; // -1h
        let secs = oauth_secs_remaining(&make_oauth_creds(ms)).unwrap();
        assert!(secs < 0, "expected negative, got {secs}");
    }

    #[test]
    fn test_oauth_secs_remaining_none_for_token() {
        let creds = r#"{"token": "sk-ant-oat01-abc"}"#;
        assert_eq!(oauth_secs_remaining(creds), None);
    }
}
