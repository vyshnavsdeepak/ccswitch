use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use colored::Colorize;
use serde::{Deserialize, Serialize};

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

// ── export ────────────────────────────────────────────────────────────────────

pub fn export(account: Option<&str>, all: bool) -> Result<()> {
    use std::io::IsTerminal;

    if all && account.is_some() {
        anyhow::bail!("--all and --account are mutually exclusive");
    }

    let seq = sequence::load()?;
    if seq.accounts.is_empty() {
        anyhow::bail!("No managed accounts found. Run `ccswitch add` first.");
    }

    // Collect account numbers to export.
    let nums: Vec<u32> = if all {
        seq.sequence.clone()
    } else if let Some(id) = account {
        let num = seq
            .resolve(id)
            .with_context(|| format!("Account '{id}' not found"))?;
        vec![num]
    } else if std::io::stdin().is_terminal() {
        // Interactive picker — show the account list and let the user choose.
        pick_accounts_interactive(&seq)?
    } else {
        let num = seq
            .active_account_number
            .context("No active account. Use --account <id> or --all.")?;
        vec![num]
    };

    // Determine active_num for the payload.
    // Only use the global active account if it is actually in the export set;
    // otherwise fall back to the first exported account (e.g. --account 2 while
    // account 1 is active should mark account 2 as active in the blob).
    let active_num = seq
        .active_account_number
        .filter(|n| nums.contains(n))
        .unwrap_or(nums[0]);

    // Build account export entries.
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

    // Compute format fingerprint from the active account's credentials.
    let format_fingerprint = account_exports
        .iter()
        .find(|a| a.num == active_num)
        .map(|a| credentials::credential_field_fingerprint(&a.credentials))
        .filter(|fp| !fp.is_empty());

    let payload = ExportPayload {
        version: 1,
        exported_at: sequence::now_utc(),
        active_num,
        format_fingerprint,
        accounts: account_exports,
    };

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
        // Clipboard not available — warn and fall back to file.
        #[cfg(not(target_os = "macos"))]
        eprintln!(
            "  {}  No clipboard tool found (tried wl-copy, xclip, xsel).",
            "⚠".yellow().bold()
        );
        write_blob_to_file(&blob)?;
    }

    Ok(())
}

// ── interactive pickers ───────────────────────────────────────────────────────

/// Print the account list and ask the user which accounts to export.
/// Returns the resolved account numbers.
fn pick_accounts_interactive(seq: &crate::sequence::SequenceFile) -> Result<Vec<u32>> {
    use std::io::Write;

    // Print account list.
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

/// Ask whether to copy to clipboard or write to a file.
/// Returns true if the user chose file.
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

/// Try to copy `blob` to the system clipboard. Returns true on success.
fn copy_to_clipboard(blob: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // Candidates: (binary, args)
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

/// Prompt for a file path and write the blob there (mode 0600).
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

    // Empty input → use the default.
    if raw.is_empty() {
        return write_blob_to_path(blob, &default_path);
    }

    // Expand a leading ~ manually (std::path doesn't do it).
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

pub fn import() -> Result<()> {
    let raw = rpassword::prompt_password("  Paste export blob: ")
        .context("Failed to read blob from terminal")?;

    let payload = parse_payload(&raw)?;

    sequence::setup_dirs()?;

    let mut seq = sequence::load().unwrap_or_default();

    // Map exported num → local num for tracking.
    let mapped_active_local = merge_sequence(&mut seq, &payload.accounts, payload.active_num);

    // Write credentials and config backups.
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
                .with_context(|| format!("Failed to set permissions on {}", config_path.display()))?;
        }
    }

    // Activate the mapped active account.
    let active_acct = payload
        .accounts
        .iter()
        .find(|a| {
            // Find the exported account whose local_num matches mapped_active_local.
            seq.find_by_email(&a.email) == Some(mapped_active_local)
        })
        .context("Cannot find the active account in the import payload")?;

    credentials::write_live(&active_acct.credentials)
        .context("Failed to write live credentials")?;

    // Ensure ~/.ccswitchrc is present so CLAUDE_CODE_OAUTH_TOKEN is unset in
    // new shells — mirrors what do_switch() does after every account activation.
    let _ = credentials::ensure_ccswitchrc();

    // Merge oauthAccount from config backup into ~/.claude/.claude.json.
    // On a fresh VM neither ~/.claude/.claude.json nor ~/.claude.json exist yet,
    // so config::load() would fail.  Fall back to an empty object so oauthAccount
    // is always written and Claude Code recognises the imported account.
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

    // Print summary.
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

// ── pure helper (also used by tests) ─────────────────────────────────────────

/// Merge imported accounts into an existing (possibly empty) `SequenceFile`.
///
/// For each account:
/// - If the email already exists locally, reuse that number.
/// - Otherwise allocate `next_account_number()`, insert `AccountEntry`, append to `sequence`.
///
/// Returns the local account number that maps to `active_num` in the payload.
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

        // Append to sequence Vec if not already present.
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
        // Pre-populate with account 5 having the same email.
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

        // Should reuse local num 5, not allocate a new one.
        assert_eq!(active_local, 5);
        assert!(seq.accounts.contains_key("5"));
        // sequence should not have duplicates.
        assert_eq!(seq.sequence.iter().filter(|&&n| n == 5).count(), 1);
    }

    #[test]
    fn test_merge_sequence_dedup_import_same_email_twice() {
        let mut seq = SequenceFile::default();
        let accounts = vec![make_account_export(1, "dup@example.com")];

        // First import.
        merge_sequence(&mut seq, &accounts, 1);
        // Second import of the same account.
        merge_sequence(&mut seq, &accounts, 1);

        // Still only one entry.
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
        assert_eq!(
            restored.format_fingerprint,
            payload.format_fingerprint
        );
    }

    #[test]
    fn test_import_version_mismatch_returns_error() {
        // Build a payload with version 99, encode it, then run it through
        // parse_payload() to verify the version guard actually fires.
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
}
