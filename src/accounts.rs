use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::{
    io::{self, Write},
    path::PathBuf,
};

use crate::{
    config, credentials,
    sequence::{self, AccountEntry, AuthKind, SequenceFile, now_utc},
};

// ── Core functions (no stdout, return descriptive string) ─────────────────────

pub(crate) fn core_add() -> Result<String> {
    sequence::setup_dirs()?;

    let email = config::current_email()
        .context("No active Claude account found. Please log in to Claude Code first.")?;

    let mut seq = sequence::load()?;

    if seq.account_exists(&email) {
        return Ok(format!("Account {} is already managed.", email));
    }

    let uuid = config::current_uuid().unwrap_or_default();
    let account_num = seq.next_account_number();
    let now = now_utc();

    let live_creds =
        credentials::read_live().context("Cannot read credentials for the current account")?;
    let live_config = config::load().context("Cannot read current Claude config")?;
    let live_config_str = serde_json::to_string_pretty(&live_config)?;

    credentials::write_backup(account_num, &email, &live_creds)?;
    write_config_backup(account_num, &email, &live_config_str)?;

    seq.accounts.insert(
        account_num.to_string(),
        AccountEntry {
            email: email.clone(),
            uuid,
            added: now.clone(),
            auth_kind: AuthKind::Oauth,
        },
    );
    seq.sequence.push(account_num);
    seq.active_account_number = Some(account_num);
    seq.last_updated = now;

    sequence::save(&seq)?;

    Ok(format!("Added {} as Account {}", email, account_num))
}

pub(crate) fn core_switch(target_num: u32) -> Result<String> {
    let mut seq = sequence::load()?;

    let target_entry = seq
        .accounts
        .get(&target_num.to_string())
        .cloned()
        .with_context(|| format!("Account {target_num} does not exist"))?;
    let target_email = target_entry.email.clone();
    let target_auth_kind = target_entry.auth_kind.clone();

    // Resolve current account — works for both OAuth (config) and token (seq state)
    let (current_num, current_slot_email) = resolve_current_account(&seq)?;

    if target_num == current_num {
        return Ok(format!(
            "Already using {} (Account {}).",
            target_email, target_num
        ));
    }

    let current_auth_kind = seq
        .accounts
        .get(&current_num.to_string())
        .map(|e| e.auth_kind.clone())
        .unwrap_or_default();

    // Step 1: Snapshot current account
    // OAuth accounts: save live credentials + config (they can be refreshed by Claude Code)
    // Token accounts: skip — the token is static and was already stored during `add`
    if current_auth_kind == AuthKind::Oauth {
        let live_creds = credentials::read_live().context("Cannot read current credentials")?;
        let live_config = config::load().context("Cannot read current Claude config")?;
        let live_config_str = serde_json::to_string_pretty(&live_config)?;

        credentials::write_backup(current_num, &current_slot_email, &live_creds)?;
        write_config_backup(current_num, &current_slot_email, &live_config_str)?;
    }

    // Step 2: Read target credentials backup
    let target_creds = credentials::read_backup(target_num, &target_email)
        .with_context(|| format!("Missing credentials backup for Account {target_num}"))?;

    // Step 3: Activate target account
    match target_auth_kind {
        AuthKind::Oauth => {
            let target_config_str = read_config_backup(target_num, &target_email)
                .with_context(|| format!("Missing config backup for Account {target_num}"))?;
            let target_config: serde_json::Value = serde_json::from_str(&target_config_str)
                .context("Invalid JSON in config backup")?;
            let target_oauth = target_config
                .get("oauthAccount")
                .cloned()
                .context("Missing oauthAccount in config backup")?;

            credentials::write_live(&target_creds).context("Failed to write credentials")?;

            let mut active_config =
                config::load().context("Cannot read live config for merge")?;
            active_config["oauthAccount"] = target_oauth;
            config::save(&active_config).context("Failed to save merged config")?;
        }
        AuthKind::Token => {
            // Update the active-token keychain slot — ~/.ccswitchrc reads from it
            let token = extract_access_token(&target_creds)?;
            credentials::write_active_token(&token)
                .context("Failed to update active-token slot")?;
        }
    }

    // Step 4: Persist updated state
    seq.active_account_number = Some(target_num);
    seq.last_updated = now_utc();
    sequence::save(&seq)?;

    Ok(format!(
        "Switched {} → {} (Account {}). Restart Claude Code to apply.",
        current_slot_email, target_email, target_num
    ))
}

pub(crate) fn core_remove(num: u32, email: &str) -> Result<String> {
    let mut seq = sequence::load()?;

    credentials::delete_backup(num, email)?;
    let _ = std::fs::remove_file(config_backup_path(num, email));

    seq.accounts.remove(&num.to_string());
    seq.sequence.retain(|&n| n != num);
    seq.last_updated = now_utc();

    sequence::save(&seq)?;

    Ok(format!("Removed Account {} ({})", num, email))
}

// ── Add current account ───────────────────────────────────────────────────────

pub fn add() -> Result<()> {
    // Route to the token flow when:
    // 1. No oauthAccount in config (pure token user), OR
    // 2. CLAUDE_CODE_OAUTH_TOKEN is set — the env var takes priority over the
    //    credentials file, so even if a stale oauthAccount exists in config,
    //    the user is effectively running in token mode.
    if config::current_email().is_none() || config::has_env_token() {
        return token_add_flow();
    }

    match core_add()? {
        msg if msg.contains("already managed") => {
            println!("  {} {}", "·".yellow(), msg);
        }
        msg => {
            println!("  {} {}", "✓".green().bold(), msg);
        }
    }
    Ok(())
}

// ── Interactive token-account add (CLI only) ──────────────────────────────────

fn token_add_flow() -> Result<()> {
    println!();
    println!(
        "  {} No active Claude account found via OAuth.",
        "·".yellow()
    );
    println!(
        "  {} Looks like you're using a long-lived token (claude setup-token).",
        "·".yellow()
    );
    println!();

    // Prompt for the token with masked input
    let token =
        rpassword::prompt_password("  Paste your token (sk-ant-oat01-...): ")
            .context("Failed to read token")?;
    let token = token.trim().to_string();

    if token.is_empty() {
        bail!("No token provided.");
    }

    // Try to extract an email hint from the token (opaque tokens → None)
    let email_hint = config::email_from_token(&token);
    let default_label = token_default_label();
    let display_default = email_hint.as_deref().unwrap_or(&default_label);

    print!("  Email / label for this account [{}]: ", display_default);
    io::stdout().flush()?;

    let mut label_input = String::new();
    io::stdin().read_line(&mut label_input)?;
    let label = label_input.trim().to_string();

    let email = if label.is_empty() {
        email_hint.unwrap_or(default_label)
    } else {
        label
    };

    // Set up dirs and check for duplicates
    sequence::setup_dirs()?;
    let mut seq = sequence::load()?;

    if seq.account_exists(&email) {
        bail!("Account {} is already managed.", email);
    }

    let account_num = seq.next_account_number();
    let now = now_utc();

    // Store token as a JSON blob so it can be round-tripped by extract_access_token
    let token_json = serde_json::json!({ "token": token }).to_string();
    credentials::write_backup(account_num, &email, &token_json)?;

    // Store a config snapshot (may lack oauthAccount — that's fine for token accounts)
    let config_backup = config::load()
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|_| "{}".to_string());
    write_config_backup(account_num, &email, &config_backup)?;

    // Write the active-token keychain/file slot
    credentials::write_active_token(&token)?;

    // Create ~/.ccswitchrc if this is the first token account
    let newly_created = credentials::ensure_ccswitchrc()?;

    // Persist to sequence
    seq.accounts.insert(
        account_num.to_string(),
        AccountEntry {
            email: email.clone(),
            uuid: String::new(),
            added: now.clone(),
            auth_kind: AuthKind::Token,
        },
    );
    seq.sequence.push(account_num);
    seq.active_account_number = Some(account_num);
    seq.last_updated = now;

    sequence::save(&seq)?;

    println!();
    println!("  {} Token stored securely.", "✓".green().bold());
    println!(
        "  {} Added {} as Account {} {}",
        "✓".green().bold(),
        email.bold(),
        account_num,
        "(token)".dimmed()
    );

    if newly_created {
        let rc_path = credentials::ccswitchrc_path();
        println!();
        println!(
            "  {}",
            "── One-time setup ──────────────────────────────────────────".dimmed()
        );
        println!(
            "  Add this line to {} (or {}):\n",
            "~/.zshrc".cyan().bold(),
            "~/.bashrc".cyan()
        );
        println!(
            "      source {}",
            rc_path.display().to_string().cyan().bold()
        );
        println!();
        println!("  Then open a new terminal — ccswitch will set");
        println!("  CLAUDE_CODE_OAUTH_TOKEN automatically on every switch.");
        println!(
            "  {}",
            "────────────────────────────────────────────────────────────".dimmed()
        );
    }

    println!();
    Ok(())
}

/// Generate a unique default label for a token account.
fn token_default_label() -> String {
    // Use a hex timestamp so each invocation gets a distinct default
    let ts = chrono::Utc::now().timestamp() as u32;
    format!("token-{:08X}", ts)
}

// ── Remove account ────────────────────────────────────────────────────────────

pub fn remove(identifier: &str) -> Result<()> {
    let seq = sequence::load()?;

    if seq.accounts.is_empty() {
        bail!("No accounts are managed yet. Run `ccswitch add` first.");
    }

    let account_num = seq
        .resolve(identifier)
        .with_context(|| format!("No account found matching '{identifier}'"))?;

    let entry = seq
        .accounts
        .get(&account_num.to_string())
        .cloned()
        .with_context(|| format!("Account {account_num} does not exist"))?;

    if seq.active_account_number == Some(account_num) {
        println!(
            "  {} Account {} ({}) is currently active.",
            "!".yellow().bold(),
            account_num,
            entry.email
        );
    }

    print!(
        "\n  Remove {} ({})? [y/N] ",
        format!("Account {account_num}").bold(),
        entry.email
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    if !matches!(input.trim(), "y" | "Y") {
        println!("  Cancelled.");
        return Ok(());
    }

    let msg = core_remove(account_num, &entry.email)?;
    println!("\n  {} {}", "✓".green().bold(), msg);
    Ok(())
}

// ── List accounts ─────────────────────────────────────────────────────────────

pub fn list() -> Result<()> {
    let seq = sequence::load()?;

    if seq.accounts.is_empty() {
        println!("\n  {}\n", "No accounts managed yet.".dimmed());
        println!("  Run {} to add the current account.\n", "ccswitch add".cyan().bold());
        return Ok(());
    }

    // Prefer seq state (works for token users without oauthAccount in config)
    let active_num = seq
        .active_account_number
        .or_else(|| config::current_email().as_deref().and_then(|e| seq.find_by_email(e)));

    println!("\n  {}", "Managed Accounts".bold());
    println!("  {}", "─".repeat(40).dimmed());

    for &num in &seq.sequence {
        let Some(entry) = seq.accounts.get(&num.to_string()) else {
            continue;
        };

        let is_active = active_num == Some(num);
        let badge = if entry.auth_kind == AuthKind::Token {
            " [token]"
        } else {
            ""
        };

        if is_active {
            println!(
                "  {}  {}{}  {}",
                format!("▶ {num:>2}").green().bold(),
                entry.email.green().bold(),
                badge.green().dimmed(),
                "(active)".green().dimmed()
            );
        } else {
            println!(
                "  {}  {}{}",
                format!("  {num:>2}").dimmed(),
                entry.email,
                badge.dimmed()
            );
        }
    }

    println!("  {}\n", "─".repeat(40).dimmed());
    Ok(())
}

// ── Status ────────────────────────────────────────────────────────────────────

pub fn status() -> Result<()> {
    let seq = sequence::load()?;

    // Resolve active account — prefer seq state so token accounts show correctly
    let active = seq
        .active_account_number
        .and_then(|num| seq.accounts.get(&num.to_string()).map(|e| (num, e.clone())))
        .or_else(|| {
            config::current_email().and_then(|email| {
                seq.find_by_email(&email)
                    .and_then(|num| seq.accounts.get(&num.to_string()).map(|e| (num, e.clone())))
            })
        });

    match active {
        None => {
            if config::has_env_token() {
                println!(
                    "\n  {} {} {}\n",
                    "·".yellow().bold(),
                    "Token active".bold(),
                    "(not managed — run `ccswitch add`)".dimmed()
                );
            } else {
                println!("\n  {} Not logged in to Claude Code.\n", "✗".red().bold());
            }
        }
        Some((num, entry)) => {
            let badge = if entry.auth_kind == AuthKind::Token {
                " [token]"
            } else {
                ""
            };
            println!(
                "\n  {} {}{} {}\n",
                "▶".green().bold(),
                entry.email.bold(),
                badge.dimmed(),
                format!("(Account {num})").dimmed()
            );
        }
    }
    Ok(())
}

// ── Switch (rotate to next) ───────────────────────────────────────────────────

pub fn switch_next() -> Result<()> {
    let seq = sequence::load()?;

    if seq.accounts.is_empty() {
        bail!("No accounts managed yet. Run `ccswitch add` first.");
    }

    if seq.sequence.len() < 2 {
        bail!("Only one account managed. Add another with `ccswitch add`.");
    }

    // For token accounts, active_account_number is the source of truth
    let active_num = if let Some(num) = seq.active_account_number {
        num
    } else {
        let current_email = config::current_email()
            .context("No active Claude account found")?;

        if !seq.account_exists(&current_email) {
            println!(
                "\n  {} Active account '{}' is not managed — adding it...",
                "·".yellow(),
                current_email
            );
            add()?;
            println!(
                "\n  Run {} again to switch to the next account.\n",
                "ccswitch switch".cyan().bold()
            );
            return Ok(());
        }

        seq.find_by_email(&current_email)
            .context("Cannot find account number for current email")?
    };

    let current_idx = seq.sequence.iter().position(|&n| n == active_num).unwrap_or(0);
    let next_idx = (current_idx + 1) % seq.sequence.len();
    let next_num = seq.sequence[next_idx];

    do_switch(next_num)
}

// ── Switch to specific account ────────────────────────────────────────────────

pub fn switch_to(identifier: &str) -> Result<()> {
    let seq = sequence::load()?;

    if seq.accounts.is_empty() {
        bail!("No accounts managed yet. Run `ccswitch add` first.");
    }

    let target_num = seq
        .resolve(identifier)
        .with_context(|| format!("No account found matching '{identifier}'"))?;

    do_switch(target_num)
}

// ── CLI switch wrapper ────────────────────────────────────────────────────────

fn do_switch(target_num: u32) -> Result<()> {
    let seq = sequence::load()?;

    let target_entry = seq
        .accounts
        .get(&target_num.to_string())
        .cloned()
        .with_context(|| format!("Account {target_num} does not exist"))?;
    let target_email = target_entry.email.clone();
    let target_is_token = target_entry.auth_kind == AuthKind::Token;

    // Determine display name for current account — handles both OAuth and token
    let current_slot_email = if let Some(num) = seq.active_account_number {
        seq.accounts
            .get(&num.to_string())
            .map(|e| e.email.clone())
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        config::current_email().unwrap_or_else(|| "unknown".to_string())
    };

    // Already on the target?
    if seq.active_account_number == Some(target_num) {
        println!(
            "\n  {} Already using {} (Account {target_num}).\n",
            "·".cyan(),
            target_email.bold()
        );
        return Ok(());
    }

    println!(
        "\n  {} {}  {}  {}",
        "→".cyan().bold(),
        current_slot_email.dimmed(),
        "→".dimmed(),
        target_email.cyan().bold()
    );

    core_switch(target_num)?;

    list()?;

    if target_is_token {
        println!(
            "  {} Restart Claude Code · open a new shell for token to take effect\n",
            "✓".green().bold()
        );
    } else {
        println!(
            "  {} Restart Claude Code to apply.\n",
            "✓".green().bold()
        );
    }

    Ok(())
}

// ── Credential helpers ────────────────────────────────────────────────────────

/// Extract the raw token value from a credentials backup.
/// Token accounts store: {"token": "sk-ant-..."}
/// OAuth accounts store the full credentials JSON (not used here).
fn extract_access_token(creds_json: &str) -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(creds_json).context("Invalid JSON in credentials backup")?;
    v.get("token")
        .and_then(|t| t.as_str())
        .map(String::from)
        .context(
            "Cannot extract token from credentials backup. \
             Was this account added with `ccswitch add`?",
        )
}

/// Resolve the currently-active account from sequence state, falling back to
/// the live Claude config. This works for both OAuth and token accounts.
fn resolve_current_account(seq: &SequenceFile) -> Result<(u32, String)> {
    if let Some(num) = seq.active_account_number {
        if let Some(entry) = seq.accounts.get(&num.to_string()) {
            return Ok((num, entry.email.clone()));
        }
    }
    let email = config::current_email()
        .context("No active Claude account found. Run `ccswitch add` first.")?;
    let num = seq
        .find_by_email(&email)
        .with_context(|| {
            format!("Account for '{email}' is not managed. Run `ccswitch add` first.")
        })?;
    Ok((num, email))
}

// ── Config backup helpers ─────────────────────────────────────────────────────

pub(crate) fn config_backup_path(num: u32, email: &str) -> PathBuf {
    sequence::backup_dir()
        .join("configs")
        .join(format!(".claude-config-{num}-{email}.json"))
}

fn write_config_backup(num: u32, email: &str, content: &str) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let path = config_backup_path(num, email);
    std::fs::write(&path, content)
        .with_context(|| format!("Cannot write config backup to {}", path.display()))?;

    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

    Ok(())
}

pub(crate) fn read_config_backup(num: u32, email: &str) -> Result<String> {
    let path = config_backup_path(num, email);
    std::fs::read_to_string(&path)
        .with_context(|| format!("Cannot read config backup from {}", path.display()))
}
