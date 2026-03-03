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
    #[cfg(test)]
    if let Ok(dir) = std::env::var("CCSWITCH_TEST_DIR") {
        return PathBuf::from(dir);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(email: &str) -> AccountEntry {
        AccountEntry {
            email: email.to_string(),
            uuid: "test-uuid".to_string(),
            added: now_utc(),
            auth_kind: AuthKind::Oauth,
        }
    }

    #[test]
    fn test_next_account_number_empty() {
        let seq = SequenceFile::default();
        assert_eq!(seq.next_account_number(), 1);
    }

    #[test]
    fn test_next_account_number_existing() {
        let mut seq = SequenceFile::default();
        seq.accounts.insert("1".into(), make_entry("a@test.com"));
        seq.accounts.insert("3".into(), make_entry("b@test.com"));
        assert_eq!(seq.next_account_number(), 4);
    }

    #[test]
    fn test_find_by_email_found() {
        let mut seq = SequenceFile::default();
        seq.accounts.insert("2".into(), make_entry("user@test.com"));
        assert_eq!(seq.find_by_email("user@test.com"), Some(2));
    }

    #[test]
    fn test_find_by_email_not_found() {
        let seq = SequenceFile::default();
        assert_eq!(seq.find_by_email("nobody@test.com"), None);
    }

    #[test]
    fn test_account_exists() {
        let mut seq = SequenceFile::default();
        seq.accounts.insert("1".into(), make_entry("user@test.com"));
        assert!(seq.account_exists("user@test.com"));
        assert!(!seq.account_exists("other@test.com"));
    }

    #[test]
    fn test_resolve_by_number() {
        let mut seq = SequenceFile::default();
        seq.accounts.insert("5".into(), make_entry("user@test.com"));
        assert_eq!(seq.resolve("5"), Some(5));
        assert_eq!(seq.resolve("6"), None);
    }

    #[test]
    fn test_resolve_by_email() {
        let mut seq = SequenceFile::default();
        seq.accounts.insert("3".into(), make_entry("user@test.com"));
        assert_eq!(seq.resolve("user@test.com"), Some(3));
        assert_eq!(seq.resolve("other@test.com"), None);
    }

    #[test]
    fn test_save_load_roundtrip() {
        let _env = crate::test_utils::TestEnv::new();
        let mut seq = SequenceFile::default();
        seq.accounts.insert("1".into(), make_entry("user@test.com"));
        seq.sequence = vec![1];
        seq.active_account_number = Some(1);
        seq.last_updated = "2024-01-01T00:00:00Z".to_string();
        save(&seq).unwrap();

        let loaded = load().unwrap();
        assert_eq!(loaded.active_account_number, Some(1));
        assert_eq!(loaded.sequence, vec![1]);
        assert_eq!(loaded.accounts["1"].email, "user@test.com");
    }

    #[test]
    fn test_load_missing_returns_default() {
        let _env = crate::test_utils::TestEnv::new();
        let seq = load().unwrap();
        assert!(seq.accounts.is_empty());
        assert_eq!(seq.active_account_number, None);
    }
}
