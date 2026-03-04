use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use colored::Colorize;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{accounts, credentials, sequence};

// ── Payload types ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ExportPayload {
    version: u8,
    exported_at: String,
    active_num: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format_fingerprint: Option<String>,
    accounts: Vec<AccountExport>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct AccountExport {
    pub(crate) num: u32,
    pub(crate) email: String,
    pub(crate) uuid: String,
    pub(crate) added: String,
    pub(crate) auth_kind: crate::sequence::AuthKind,
    /// Raw JSON string of the credentials blob.
    pub(crate) credentials: String,
    /// Raw JSON string of the config backup. Defaults to "{}".
    #[serde(default = "default_empty_object")]
    pub(crate) config: String,
}

fn default_empty_object() -> String {
    "{}".to_string()
}

// ── crypto helpers ─────────────────────────────────────────────────────────────

/// Encrypt `plaintext` with passphrase using PBKDF2-SHA256 + ChaCha20-Poly1305.
/// Returns `base64(salt[16] ++ nonce[12] ++ ciphertext+tag)`.
fn encrypt(plaintext: &[u8], passphrase: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);

    let mut key_bytes = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), &salt, 100_000, &mut key_bytes);

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    let mut bundle = Vec::with_capacity(16 + 12 + ciphertext.len());
    bundle.extend_from_slice(&salt);
    bundle.extend_from_slice(&nonce_bytes);
    bundle.extend_from_slice(&ciphertext);

    Ok(STANDARD.encode(&bundle))
}

/// Decrypt a bundle produced by `encrypt()`.
fn decrypt(encoded: &str, passphrase: &str) -> Result<Vec<u8>> {
    let bundle = STANDARD
        .decode(encoded.trim().as_bytes())
        .context("Invalid base64 in encrypted blob")?;

    // minimum: 16 (salt) + 12 (nonce) + 16 (Poly1305 tag, empty plaintext)
    if bundle.len() < 44 {
        anyhow::bail!("Encrypted blob is too short");
    }

    let (salt, rest) = bundle.split_at(16);
    let (nonce_bytes, ciphertext) = rest.split_at(12);

    let mut key_bytes = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, 100_000, &mut key_bytes);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("Decryption failed — wrong passphrase?"))
}

// ── GitHub CLI helper ──────────────────────────────────────────────────────────

fn gh_token() -> Result<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("Failed to run `gh auth token` — is the GitHub CLI installed?\nInstall from https://cli.github.com")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "GitHub CLI not authenticated. Run `gh auth login` first.\n{}",
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ── export ────────────────────────────────────────────────────────────────────

/// Build the export payload (accounts list + metadata) without deciding where to send it.
fn build_export_payload(account: Option<&str>, all: bool) -> Result<ExportPayload> {
    #[allow(unused_imports)]
    use std::io::IsTerminal;

    if all && account.is_some() {
        anyhow::bail!("--all and --account are mutually exclusive");
    }

    let seq = sequence::load()?;
    if seq.accounts.is_empty() {
        anyhow::bail!("No managed accounts found. Run `ccswitch add` first.");
    }

    let nums: Vec<u32> = if all {
        seq.sequence.clone()
    } else if let Some(id) = account {
        let num = seq
            .resolve(id)
            .with_context(|| format!("Account '{id}' not found"))?;
        vec![num]
    } else if std::io::stdin().is_terminal() {
        pick_accounts_interactive(&seq)?
    } else {
        let num = seq
            .active_account_number
            .context("No active account. Use --account <id> or --all.")?;
        vec![num]
    };

    let active_num = seq
        .active_account_number
        .filter(|n| nums.contains(n))
        .unwrap_or(nums[0]);

    let mut account_exports: Vec<AccountExport> = Vec::new();
    for &num in &nums {
        let entry = seq
            .accounts
            .get(&num.to_string())
            .with_context(|| format!("Account {num} not found in sequence"))?;

        let creds = credentials::read_backup(num, &entry.email)
            .with_context(|| format!("Cannot read credentials backup for Account {num}"))?;

        let config = accounts::read_config_backup(num, &entry.email)
            .unwrap_or_else(|_| "{}".to_string());

        account_exports.push(AccountExport {
            num,
            email: entry.email.clone(),
            uuid: entry.uuid.clone(),
            added: entry.added.clone(),
            auth_kind: entry.auth_kind.clone(),
            credentials: creds,
            config,
        });
    }

    let format_fingerprint = account_exports
        .iter()
        .find(|a| a.num == active_num)
        .map(|a| credentials::credential_field_fingerprint(&a.credentials))
        .filter(|fp| !fp.is_empty());

    Ok(ExportPayload {
        version: 1,
        exported_at: sequence::now_utc(),
        active_num,
        format_fingerprint,
        accounts: account_exports,
    })
}

pub fn export(account: Option<&str>, all: bool) -> Result<()> {
    use std::io::IsTerminal;
    let payload = build_export_payload(account, all)?;
    let json = serde_json::to_string(&payload).context("Failed to serialize export payload")?;
    let blob = STANDARD.encode(json.as_bytes());

    println!();
    let use_file = if std::io::stdin().is_terminal() {
        pick_destination_interactive()
    } else {
        false
    };

    if use_file {
        write_blob_to_file(&blob)?;
    } else if copy_to_clipboard(&blob) {
        println!(
            "  {}  Copied to clipboard — run {} on the remote and paste.\n",
            "✓".green().bold(),
            "ccswitch import".cyan().bold()
        );
    } else {
        #[cfg(not(target_os = "macos"))]
        eprintln!(
            "  {}  No clipboard tool found (tried wl-copy, xclip, xsel).",
            "⚠".yellow().bold()
        );
        write_blob_to_file(&blob)?;
    }

    Ok(())
}

pub fn export_gist(account: Option<&str>, all: bool) -> Result<()> {
    let payload = build_export_payload(account, all)?;
    let json = serde_json::to_string(&payload).context("Failed to serialize export payload")?;

    let passphrase = rpassword::prompt_password("  Passphrase (to encrypt): ")
        .context("Failed to read passphrase")?;
    if passphrase.is_empty() {
        anyhow::bail!("Passphrase must not be empty");
    }

    let encrypted = encrypt(json.as_bytes(), &passphrase)?;

    let token = gh_token()?;

    let resp = ureq::post("https://api.github.com/gists")
        .set("Authorization", &format!("Bearer {}", token))
        .set("User-Agent", "ccswitch")
        .send_json(serde_json::json!({
            "description": "ccswitch-export (delete after use)",
            "public": false,
            "files": {
                "ccswitch.blob": { "content": encrypted }
            }
        }))
        .context("Failed to create GitHub Gist")?;

    let json_resp: serde_json::Value = resp.into_json().context("Invalid JSON from gist API")?;
    let gist_id = json_resp["id"]
        .as_str()
        .context("Missing 'id' in gist creation response")?;

    println!(
        "\n  {}  Uploaded — import with:\n\n      {}\n",
        "✓".green().bold(),
        format!("ccswitch import --gist {}", gist_id).cyan().bold()
    );

    Ok(())
}

// ── interactive pickers ───────────────────────────────────────────────────────

fn pick_accounts_interactive(seq: &crate::sequence::SequenceFile) -> Result<Vec<u32>> {
    use std::io::Write;

    println!("  {}\n", "Accounts:".bold());
    for &num in &seq.sequence {
        let Some(entry) = seq.accounts.get(&num.to_string()) else {
            continue;
        };
        let active_marker = if seq.active_account_number == Some(num) {
            format!("▶ {:>2}", num).green().bold().to_string()
        } else {
            format!("  {:>2}", num).dimmed().to_string()
        };
        println!("  {}  {}", active_marker, entry.email);
    }

    println!();
    print!(
        "  Export which account? [{} for all, Enter for active]: ",
        "all".cyan()
    );
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("all") {
        return Ok(seq.sequence.clone());
    }

    if input.is_empty() {
        let num = seq
            .active_account_number
            .context("No active account. Specify an account number.")?;
        return Ok(vec![num]);
    }

    let num = seq
        .resolve(input)
        .with_context(|| format!("Account '{input}' not found"))?;
    Ok(vec![num])
}

fn pick_destination_interactive() -> bool {
    use std::io::Write;

    print!(
        "  Destination? [{} / {}] [default: clipboard]: ",
        "c".cyan().bold(),
        "f".cyan().bold()
    );
    let _ = std::io::stdout().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "f" | "file")
}

// ── clipboard / file helpers ──────────────────────────────────────────────────

fn copy_to_clipboard(blob: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];

    #[cfg(not(target_os = "macos"))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    for (bin, args) in candidates {
        let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(blob.as_bytes()).is_err() {
                continue;
            }
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn write_blob_to_file(blob: &str) -> Result<()> {
    use std::io::Write;

    let default_path = dirs::home_dir()
        .context("Cannot find home directory")?
        .join("ccswitch-export.blob");
    let default_str = default_path.display().to_string();

    print!("  File path [{}]: ", default_str.dimmed());
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let raw = input.trim();

    if raw.is_empty() {
        return write_blob_to_path(blob, &default_path);
    }

    let path = if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .context("Cannot find home directory")?
            .join(rest)
    } else {
        std::path::PathBuf::from(raw)
    };

    write_blob_to_path(blob, &path)
}

fn write_blob_to_path(blob: &str, path: &std::path::Path) -> Result<()> {
    std::fs::write(path, blob)
        .with_context(|| format!("Cannot write to {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    println!(
        "  {}  Written to {} — keep it secret and delete after use.\n",
        "✓".green().bold(),
        path.display().to_string().cyan()
    );
    println!(
        "  {}  Run {} on the remote, then: {}\n",
        "·".dimmed(),
        "ccswitch import".cyan().bold(),
        format!("rm {}", path.display()).dimmed()
    );

    Ok(())
}

// ── import ────────────────────────────────────────────────────────────────────

fn parse_payload(blob: &str) -> Result<ExportPayload> {
    let decoded = STANDARD
        .decode(blob.trim().as_bytes())
        .context("Invalid base64 — make sure you pasted the complete blob")?;
    let payload: ExportPayload = serde_json::from_slice(&decoded)
        .context("Failed to parse export blob — it may be corrupted or from an incompatible version")?;
    if payload.version != 1 {
        anyhow::bail!(
            "Unsupported export version {} (this version of ccswitch only supports version 1)",
            payload.version
        );
    }
    Ok(payload)
}

/// Apply an already-parsed export payload: write credentials, update sequence, activate account.
fn do_import(payload: ExportPayload) -> Result<()> {
    sequence::setup_dirs()?;

    let mut seq = sequence::load().unwrap_or_default();

    let mapped_active_local = merge_sequence(&mut seq, &payload.accounts, payload.active_num);

    for acct in &payload.accounts {
        let local_num = seq.find_by_email(&acct.email).unwrap_or(1);
        credentials::write_backup(local_num, &acct.email, &acct.credentials)
            .with_context(|| format!("Failed to write credentials for {}", acct.email))?;

        let config_path = accounts::config_backup_path(local_num, &acct.email);
        std::fs::write(&config_path, &acct.config)
            .with_context(|| format!("Failed to write config backup for {}", acct.email))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| {
                    format!("Failed to set permissions on {}", config_path.display())
                })?;
        }
    }

    let active_acct = payload
        .accounts
        .iter()
        .find(|a| seq.find_by_email(&a.email) == Some(mapped_active_local))
        .context("Cannot find the active account in the import payload")?;

    credentials::write_live(&active_acct.credentials)
        .context("Failed to write live credentials")?;

    let _ = credentials::ensure_ccswitchrc();

    if let Ok(config_json) = serde_json::from_str::<serde_json::Value>(&active_acct.config) {
        if let Some(oauth_account) = config_json.get("oauthAccount").cloned() {
            let mut live_config = crate::config::load().unwrap_or_else(|_| serde_json::json!({}));
            live_config["oauthAccount"] = oauth_account;
            let _ = crate::config::save(&live_config);
        }
    }

    seq.active_account_number = Some(mapped_active_local);
    seq.last_updated = sequence::now_utc();
    sequence::save(&seq)?;

    println!();
    for acct in &payload.accounts {
        let local_num = seq.find_by_email(&acct.email).unwrap_or(mapped_active_local);
        let is_active = local_num == mapped_active_local;
        if is_active {
            println!(
                "  {}  Imported {} (Account {}) — active",
                "✓".green().bold(),
                acct.email,
                local_num
            );
        } else {
            println!(
                "  {}  Imported {} (Account {})",
                "✓".green().bold(),
                acct.email,
                local_num
            );
        }
    }
    println!("\n  {}  Restart Claude Code to apply.\n", "✓".green().bold());

    Ok(())
}

pub fn import() -> Result<()> {
    let raw = rpassword::prompt_password("  Paste export blob: ")
        .context("Failed to read blob from terminal")?;

    let payload = parse_payload(&raw)?;
    do_import(payload)
}

pub fn import_gist(id: &str) -> Result<()> {
    let token = gh_token()?;

    let url = format!("https://api.github.com/gists/{}", id);

    let resp = match ureq::get(&url)
        .set("Authorization", &format!("Bearer {}", token))
        .set("User-Agent", "ccswitch")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => {
            anyhow::bail!(
                "Gist '{}' not found — has it already been deleted or is the ID wrong?",
                id
            );
        }
        Err(e) => return Err(e.into()),
    };

    let json_resp: serde_json::Value = resp.into_json().context("Invalid JSON from gist API")?;
    let encrypted = json_resp["files"]["ccswitch.blob"]["content"]
        .as_str()
        .context("Gist does not contain a 'ccswitch.blob' file — is this a ccswitch gist?")?;

    let passphrase = rpassword::prompt_password("  Passphrase (to decrypt): ")
        .context("Failed to read passphrase")?;

    let plaintext = decrypt(encrypted, &passphrase)?;

    let blob = STANDARD.encode(&plaintext);
    let payload = parse_payload(&blob)?;

    do_import(payload)?;

    // Delete the gist only after a successful import.
    match ureq::delete(&url)
        .set("Authorization", &format!("Bearer {}", token))
        .set("User-Agent", "ccswitch")
        .call()
    {
        Ok(_) => println!("  {}  Gist deleted.\n", "✓".green().bold()),
        Err(e) => eprintln!(
            "  {}  Could not delete gist {} — remove it manually: {}\n",
            "⚠".yellow().bold(),
            id,
            e
        ),
    }

    Ok(())
}

// ── pure helper (also used by tests) ─────────────────────────────────────────

pub(crate) fn merge_sequence(
    seq: &mut crate::sequence::SequenceFile,
    accounts: &[AccountExport],
    active_num: u32,
) -> u32 {
    let mut active_local = 1u32;

    for acct in accounts {
        let local_num = if let Some(existing) = seq.find_by_email(&acct.email) {
            existing
        } else {
            let new_num = seq.next_account_number();
            seq.accounts.insert(
                new_num.to_string(),
                crate::sequence::AccountEntry {
                    email: acct.email.clone(),
                    uuid: acct.uuid.clone(),
                    added: acct.added.clone(),
                    auth_kind: acct.auth_kind.clone(),
                },
            );
            new_num
        };

        if !seq.sequence.contains(&local_num) {
            seq.sequence.push(local_num);
        }

        if acct.num == active_num {
            active_local = local_num;
        }
    }

    active_local
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequence::{AccountEntry, AuthKind, SequenceFile};

    fn make_account_export(num: u32, email: &str) -> AccountExport {
        AccountExport {
            num,
            email: email.to_string(),
            uuid: format!("uuid-{num}"),
            added: "2026-01-01T00:00:00Z".to_string(),
            auth_kind: AuthKind::Oauth,
            credentials: r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"rtok","expiresAt":9999999999999,"scopes":[]}}"#.to_string(),
            config: "{}".to_string(),
        }
    }

    #[test]
    fn test_merge_sequence_empty_local_assigns_num_1() {
        let mut seq = SequenceFile::default();
        let accounts = vec![make_account_export(1, "user@example.com")];
        let active_local = merge_sequence(&mut seq, &accounts, 1);
        assert_eq!(active_local, 1);
        assert!(seq.accounts.contains_key("1"));
        assert_eq!(seq.sequence, vec![1]);
    }

    #[test]
    fn test_merge_sequence_reuses_existing_email() {
        let mut seq = SequenceFile::default();
        seq.accounts.insert(
            "5".to_string(),
            AccountEntry {
                email: "existing@example.com".to_string(),
                uuid: "old-uuid".to_string(),
                added: "2025-01-01T00:00:00Z".to_string(),
                auth_kind: AuthKind::Oauth,
            },
        );
        seq.sequence.push(5);

        let accounts = vec![make_account_export(1, "existing@example.com")];
        let active_local = merge_sequence(&mut seq, &accounts, 1);

        assert_eq!(active_local, 5);
        assert!(seq.accounts.contains_key("5"));
        assert_eq!(seq.sequence.iter().filter(|&&n| n == 5).count(), 1);
    }

    #[test]
    fn test_merge_sequence_dedup_import_same_email_twice() {
        let mut seq = SequenceFile::default();
        let accounts = vec![make_account_export(1, "dup@example.com")];

        merge_sequence(&mut seq, &accounts, 1);
        merge_sequence(&mut seq, &accounts, 1);

        assert_eq!(seq.sequence.iter().filter(|&&n| n == 1).count(), 1);
        assert_eq!(seq.accounts.len(), 1);
    }

    #[test]
    fn test_export_payload_serde_roundtrip() {
        let payload = ExportPayload {
            version: 1,
            exported_at: "2026-03-03T12:00:00Z".to_string(),
            active_num: 1,
            format_fingerprint: Some("accessToken|expiresAt|refreshToken|scopes".to_string()),
            accounts: vec![make_account_export(1, "round@example.com")],
        };

        let json = serde_json::to_string(&payload).unwrap();
        let blob = STANDARD.encode(json.as_bytes());

        let decoded = STANDARD.decode(blob.as_bytes()).unwrap();
        let restored: ExportPayload = serde_json::from_slice(&decoded).unwrap();

        assert_eq!(restored.version, payload.version);
        assert_eq!(restored.active_num, payload.active_num);
        assert_eq!(restored.accounts[0].email, payload.accounts[0].email);
        assert_eq!(restored.format_fingerprint, payload.format_fingerprint);
    }

    #[test]
    fn test_import_version_mismatch_returns_error() {
        let payload = ExportPayload {
            version: 99,
            exported_at: "2026-03-03T12:00:00Z".to_string(),
            active_num: 1,
            format_fingerprint: None,
            accounts: vec![make_account_export(1, "v99@example.com")],
        };
        let json = serde_json::to_string(&payload).unwrap();
        let blob = STANDARD.encode(json.as_bytes());

        let result = parse_payload(&blob);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported export version 99"));
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let plaintext = b"hello, this is a secret payload!";
        let passphrase = "correct-horse-battery-staple";
        let encoded = encrypt(plaintext, passphrase).unwrap();
        let decrypted = decrypt(&encoded, passphrase).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_decrypt_wrong_passphrase_fails() {
        let plaintext = b"secret credential data";
        let encoded = encrypt(plaintext, "correct-passphrase").unwrap();
        let result = decrypt(&encoded, "wrong-passphrase");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Decryption failed"));
    }

    #[test]
    fn test_encrypt_produces_different_ciphertext_each_time() {
        let plaintext = b"same input";
        let passphrase = "same passphrase";
        let enc1 = encrypt(plaintext, passphrase).unwrap();
        let enc2 = encrypt(plaintext, passphrase).unwrap();
        // Random salt + nonce means output must differ.
        assert_ne!(enc1, enc2);
        // Both must decrypt correctly.
        assert_eq!(decrypt(&enc1, passphrase).unwrap(), plaintext);
        assert_eq!(decrypt(&enc2, passphrase).unwrap(), plaintext);
    }
}
