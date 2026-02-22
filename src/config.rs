use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs, path::PathBuf};

/// Find the active Claude config file: prefers ~/.claude/.claude.json if it has
/// an oauthAccount, falls back to ~/.claude.json.
pub fn path() -> PathBuf {
    let home = dirs::home_dir().expect("Cannot find home directory");
    let primary = home.join(".claude").join(".claude.json");
    let fallback = home.join(".claude.json");

    if primary.exists() {
        if let Ok(content) = fs::read_to_string(&primary) {
            if let Ok(v) = serde_json::from_str::<Value>(&content) {
                if v.get("oauthAccount").is_some() {
                    return primary;
                }
            }
        }
    }
    fallback
}

pub fn load() -> Result<Value> {
    let p = path();
    let content = fs::read_to_string(&p)
        .with_context(|| format!("Cannot read Claude config at {}", p.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Invalid JSON in {}", p.display()))
}

pub fn save(config: &Value) -> Result<()> {
    let p = path();
    let content = serde_json::to_string_pretty(config)?;
    crate::sequence::write_atomic(&p, &content)
}

pub fn current_email() -> Option<String> {
    load().ok().and_then(|v| {
        v.get("oauthAccount")?
            .get("emailAddress")?
            .as_str()
            .map(String::from)
    })
}

pub fn current_uuid() -> Option<String> {
    load().ok().and_then(|v| {
        v.get("oauthAccount")?
            .get("accountUuid")?
            .as_str()
            .map(String::from)
    })
}

/// True if CLAUDE_CODE_OAUTH_TOKEN env var is currently set (token-based auth).
pub fn has_env_token() -> bool {
    std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok()
}

/// Try to extract an email/label from a token string.
/// Claude tokens (sk-ant-oat01-...) are opaque, so this returns None;
/// the caller will prompt the user for a label.
pub fn email_from_token(_token: &str) -> Option<String> {
    None
}
