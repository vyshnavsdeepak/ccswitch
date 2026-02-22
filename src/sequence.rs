use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io::Write, path::PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    #[default]
    Oauth,
    Token,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AccountEntry {
    pub email: String,
    pub uuid: String,
    pub added: String,
    #[serde(default)]
    pub auth_kind: AuthKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SequenceFile {
    #[serde(rename = "activeAccountNumber")]
    pub active_account_number: Option<u32>,
    #[serde(rename = "lastUpdated")]
    pub last_updated: String,
    pub sequence: Vec<u32>,
    pub accounts: HashMap<String, AccountEntry>,
}

impl SequenceFile {
    pub fn next_account_number(&self) -> u32 {
        self.accounts
            .keys()
            .filter_map(|k| k.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn find_by_email(&self, email: &str) -> Option<u32> {
        self.accounts
            .iter()
            .find(|(_, v)| v.email == email)
            .and_then(|(k, _)| k.parse().ok())
    }

    pub fn account_exists(&self, email: &str) -> bool {
        self.accounts.values().any(|a| a.email == email)
    }

    /// Resolve an account identifier (number string or email) to an account number.
    pub fn resolve(&self, identifier: &str) -> Option<u32> {
        if let Ok(num) = identifier.parse::<u32>() {
            if self.accounts.contains_key(&num.to_string()) {
                return Some(num);
            }
            None
        } else {
            self.find_by_email(identifier)
        }
    }
}

pub fn now_utc() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn backup_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Cannot find home directory")
        .join(".claude-switch-backup")
}

pub fn sequence_path() -> PathBuf {
    backup_dir().join("sequence.json")
}

pub fn setup_dirs() -> Result<()> {
    let base = backup_dir();
    fs::create_dir_all(base.join("configs"))?;
    fs::create_dir_all(base.join("credentials"))?;

    #[cfg(unix)]
    {
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(base.join("configs"), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(base.join("credentials"), fs::Permissions::from_mode(0o700))?;
    }

    Ok(())
}

pub fn load() -> Result<SequenceFile> {
    let path = sequence_path();
    if !path.exists() {
        return Ok(SequenceFile::default());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Invalid JSON in {}", path.display()))
}

pub fn save(seq: &SequenceFile) -> Result<()> {
    let path = sequence_path();
    let content = serde_json::to_string_pretty(seq)?;
    write_atomic(&path, &content)
}

/// Atomically write a JSON file: validate → temp file → rename → chmod 600.
pub fn write_atomic(path: &PathBuf, content: &str) -> Result<()> {
    // Validate JSON before touching the real file
    let _: serde_json::Value =
        serde_json::from_str(content).context("Refusing to write invalid JSON")?;

    let temp_path = path.with_extension(format!("tmp.{}", std::process::id()));

    {
        let mut f = fs::File::create(&temp_path)
            .with_context(|| format!("Cannot create temp file {}", temp_path.display()))?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
    }

    fs::rename(&temp_path, path)
        .with_context(|| format!("Cannot finalize file at {}", path.display()))?;

    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

    Ok(())
}
