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

    // Record the credential format fingerprint so future switches/refreshes
    // can detect if Claude Code has changed its credential schema.
    let fp = credentials::credential_field_fingerprint(&live_creds);
    if !fp.is_empty() {
        seq.format_fingerprint = Some(fp);
    }

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
        warn_if_format_changed(&seq, &live_creds);
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
            let token = extract_access_token(&target_creds)?;
            // Write directly to the live credentials keychain so Claude Code
            // picks it up on next restart — no CLAUDE_CODE_OAUTH_TOKEN needed.
            credentials::write_live_token(&token)
                .context("Failed to write token to live credentials")?;
            // Keep ccswitch-active-token updated for verification purposes.
            let _ = credentials::write_active_token(&token);
            // Clear oauthAccount from config — token accounts have no profile.
            if let Ok(mut cfg) = config::load() {
                if let Some(obj) = cfg.as_object_mut() {
                    obj.remove("oauthAccount");
                }
                let _ = config::save(&cfg);
            }
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

    // If the token is already in the environment, use it directly — no need to paste.
    let token = if let Ok(env_token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        let t = env_token.trim().to_string();
        if !t.is_empty() {
            println!("  {} Using token from $CLAUDE_CODE_OAUTH_TOKEN.", "·".cyan());
            println!();
            t
        } else {
            prompt_token()?
        }
    } else {
        prompt_token()?
    };

    if token.is_empty() {
        bail!("No token provided.");
    }

    // Check whether this token is already managed (by value, not just label)
    sequence::setup_dirs()?;
    let mut seq = sequence::load()?;

    let token = if let Some((existing_num, existing_email)) = find_account_by_token(&seq, &token) {
        println!(
            "  {} Already managed as {} {}",
            "·".yellow(),
            existing_email.bold(),
            format!("(Account {})", existing_num).dimmed()
        );
        println!();
        let new = rpassword::prompt_password(
            "  Paste a different token to add another account (Enter to cancel): ",
        )?;
        let new = new.trim().to_string();
        if new.is_empty() {
            return Ok(());
        }
        if let Some((n2, e2)) = find_account_by_token(&seq, &new) {
            bail!(
                "That token is also already managed as {} (Account {}).",
                e2,
                n2
            );
        }
        new
    } else {
        token
    };

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

    // Check for duplicate label
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

    // Write token to the live credentials keychain so Claude Code reads it
    // directly — no CLAUDE_CODE_OAUTH_TOKEN env var needed.
    credentials::write_live_token(&token)?;
    // Also keep ccswitch-active-token for verification.
    let _ = credentials::write_active_token(&token);

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
        println!("  This clears CLAUDE_CODE_OAUTH_TOKEN so Claude Code");
        println!("  reads credentials from the keychain on every restart.");
        println!(
            "  {}",
            "────────────────────────────────────────────────────────────".dimmed()
        );
    }

    println!();
    Ok(())
}

fn prompt_token() -> Result<String> {
    let token = rpassword::prompt_password("  Paste your token (sk-ant-oat01-...): ")
        .context("Failed to read token")?;
    Ok(token.trim().to_string())
}

/// Generate a unique default label for a token account.
fn token_default_label() -> String {
    // Use a hex timestamp so each invocation gets a distinct default
    let ts = chrono::Utc::now().timestamp() as u32;
    format!("token-{:08X}", ts)
}

// ── Refresh OAuth token ───────────────────────────────────────────────────────

pub(crate) fn core_refresh(target_num: u32) -> Result<String> {
    let seq = sequence::load()?;

    let entry = seq
        .accounts
        .get(&target_num.to_string())
        .cloned()
        .with_context(|| format!("Account {target_num} does not exist"))?;

    if entry.auth_kind != AuthKind::Oauth {
        return Ok(format!(
            "Account {} uses a static token — nothing to refresh.",
            entry.email
        ));
    }

    let is_active = seq.active_account_number == Some(target_num);

    let creds = if is_active {
        credentials::read_live().context("Cannot read current credentials")?
    } else {
        credentials::read_backup(target_num, &entry.email)
            .with_context(|| format!("Cannot read backup credentials for Account {target_num}"))?
    };
    warn_if_format_changed(&seq, &creds);

    let new_creds = credentials::refresh_oauth_creds(&creds).map_err(|e| {
        // Distinguish between a bad refresh token (needs full re-login) and network/other errors
        let msg = e.to_string();
        if msg.contains("invalid_grant") || msg.contains("not found or invalid") {
            anyhow::anyhow!(
                "Refresh token expired for Account {} ({}).\n  \
                 Re-login: switch to this account (`ccswitch switch {}`), \
                 then run `claude` to authenticate and `ccswitch add` to save the new session.",
                target_num, entry.email, target_num
            )
        } else {
            anyhow::anyhow!("Failed to refresh token for Account {target_num}: {e}")
        }
    })?;

    credentials::write_backup(target_num, &entry.email, &new_creds)?;
    if is_active {
        credentials::write_live(&new_creds).context("Failed to write refreshed credentials")?;
    }

    Ok(format!("Refreshed token for Account {} ({})", target_num, entry.email))
}

pub fn refresh(identifier: Option<&str>, all: bool) -> Result<()> {
    if all && identifier.is_some() {
        bail!("--all cannot be combined with a specific account identifier.");
    }

    let seq = sequence::load()?;

    if seq.accounts.is_empty() {
        bail!("No accounts managed yet. Run `ccswitch add` first.");
    }

    if all {
        return refresh_all(&seq);
    }

    let target_num = if let Some(id) = identifier {
        seq.resolve(id)
            .with_context(|| format!("No account found matching '{id}'"))?
    } else {
        seq.active_account_number
            .or_else(|| {
                config::current_email()
                    .as_deref()
                    .and_then(|e| seq.find_by_email(e))
            })
            .context("No active account found. Pass an account number or email.")?
    };

    println!();
    let msg = core_refresh(target_num)?;
    println!("  {} {}\n", "✓".green().bold(), msg);
    Ok(())
}

fn refresh_all(seq: &SequenceFile) -> Result<()> {
    const THRESHOLD_SECS: i64 = 24 * 3600;

    let mut n_refreshed = 0u32;
    let mut n_skipped = 0u32;
    let mut failures: Vec<String> = vec![];

    println!();

    for &num in &seq.sequence {
        let Some(entry) = seq.accounts.get(&num.to_string()) else {
            continue;
        };

        if entry.auth_kind == AuthKind::Token {
            println!(
                "  {}  Account {} ({}) — skipped (token account)",
                "·".dimmed(),
                num,
                entry.email.dimmed()
            );
            n_skipped += 1;
            continue;
        }

        let is_active = seq.active_account_number == Some(num);
        let creds_result = if is_active {
            credentials::read_live()
        } else {
            credentials::read_backup(num, &entry.email)
        };

        let needs_refresh = match creds_result {
            Err(_) => true,
            Ok(ref c) => credentials::oauth_secs_remaining(c)
                .map_or(false, |secs| secs <= THRESHOLD_SECS),
        };

        if !needs_refresh {
            println!(
                "  {}  Account {} ({}) — healthy, skipped",
                "·".dimmed(),
                num,
                entry.email.dimmed()
            );
            n_skipped += 1;
            continue;
        }

        match core_refresh(num) {
            Ok(msg) => {
                println!("  {}  {}", "✓".green().bold(), msg);
                n_refreshed += 1;
            }
            Err(e) => {
                let first_line = e.to_string();
                let first_line = first_line.lines().next().unwrap_or("error");
                println!(
                    "  {}  Account {} ({}) — {}",
                    "✗".red().bold(),
                    num,
                    entry.email,
                    first_line
                );
                failures.push(format!("Account {} ({}): {}", num, entry.email, e));
            }
        }
    }

    println!();
    println!(
        "  Summary — refreshed: {}  skipped: {}  failed: {}",
        n_refreshed.to_string().bold(),
        n_skipped.to_string().dimmed(),
        if failures.is_empty() {
            "0".normal()
        } else {
            failures.len().to_string().red().bold()
        }
    );
    println!();

    if !failures.is_empty() {
        bail!("{} account(s) failed to refresh", failures.len());
    }

    Ok(())
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

// ── Alias ─────────────────────────────────────────────────────────────────────

pub fn set_alias(account: &str, name: &str) -> Result<()> {
    let mut seq = sequence::load()?;

    if seq.accounts.is_empty() {
        bail!("No accounts managed yet. Run `ccswitch add` first.");
    }

    let num = seq
        .resolve(account)
        .with_context(|| format!("No account found matching '{account}'"))?;

    if let Some(&existing_num) = seq.aliases.get(name) {
        bail!("Alias '{}' is already used by Account {}", name, existing_num);
    }

    let email = seq.accounts[&num.to_string()].email.clone();

    seq.aliases.insert(name.to_string(), num);
    seq.last_updated = sequence::now_utc();
    sequence::save(&seq)?;

    println!(
        "\n  {} Alias '{}' → Account {} ({})\n",
        "✓".green().bold(),
        name,
        num,
        email
    );
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

    // Build reverse alias map: account_num -> alias name
    let alias_for: std::collections::HashMap<u32, &str> = seq
        .aliases
        .iter()
        .map(|(name, &num)| (num, name.as_str()))
        .collect();

    for &num in &seq.sequence {
        let Some(entry) = seq.accounts.get(&num.to_string()) else {
            continue;
        };

        let is_active = active_num == Some(num);

        // Read credentials to check session expiry (best-effort; skip on error)
        let expiry_badge: Option<String> = if entry.auth_kind == AuthKind::Oauth {
            let creds = if is_active {
                credentials::read_live().ok()
            } else {
                credentials::read_backup(num, &entry.email).ok()
            };
            creds.map(|c| session_expiry_badge(&c))
        } else {
            None
        };

        let kind_badge = if entry.auth_kind == AuthKind::Token {
            " [token]"
        } else {
            ""
        };

        let alias_badge = alias_for
            .get(&num)
            .map(|a| format!(" [{}]", a))
            .unwrap_or_default();

        if is_active {
            print!(
                "  {}  {}{}{}",
                format!("▶ {num:>2}").green().bold(),
                entry.email.green().bold(),
                kind_badge.green().dimmed(),
                alias_badge.green().dimmed(),
            );
            if let Some(ref eb) = expiry_badge {
                if eb.starts_with("[expired]") {
                    print!("  {}", eb.red().bold());
                } else if !eb.is_empty() {
                    print!("  {}", eb.yellow());
                }
            }
            println!("  {}", "(active)".green().dimmed());
        } else {
            print!(
                "  {}  {}{}{}",
                format!("  {num:>2}").dimmed(),
                entry.email,
                kind_badge.dimmed(),
                alias_badge.dimmed(),
            );
            if let Some(ref eb) = expiry_badge {
                if eb.starts_with("[expired]") {
                    print!("  {}", eb.red().bold());
                } else if !eb.is_empty() {
                    print!("  {}", eb.yellow());
                }
            }
            println!();
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
            let kind_badge = if entry.auth_kind == AuthKind::Token {
                " [token]"
            } else {
                ""
            };

            let expiry_str = if entry.auth_kind == AuthKind::Oauth {
                credentials::read_live().ok().map(|c| {
                    match credentials::oauth_secs_remaining(&c) {
                        None => String::new(),
                        Some(secs) if secs <= 0 => " — session expired".red().bold().to_string(),
                        Some(secs) => {
                            let days = secs / 86400;
                            let hours = (secs % 86400) / 3600;
                            if days > 7 {
                                format!(" — expires in {}d", days).dimmed().to_string()
                            } else if days >= 1 {
                                format!(" — expires in {}d {}h", days, hours).yellow().to_string()
                            } else {
                                format!(" — expires in {}h", hours).yellow().bold().to_string()
                            }
                        }
                    }
                })
            } else {
                None
            };

            println!(
                "\n  {} {}{}{} {}\n",
                "▶".green().bold(),
                entry.email.bold(),
                kind_badge.dimmed(),
                expiry_str.as_deref().unwrap_or(""),
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

    // If the target is an OAuth account with an expired session, warn and
    // optionally refresh before switching.
    if target_entry.auth_kind == AuthKind::Oauth {
        if let Ok(backup_creds) = credentials::read_backup(target_num, &target_email) {
            if !credentials::is_oauth_active(&backup_creds) {
                println!(
                    "\n  {} Account {} ({}) has an expired session.",
                    "!".yellow().bold(),
                    target_num,
                    target_email
                );

                use std::io::IsTerminal;
                if io::stdin().is_terminal() {
                    print!("  Refresh now? [y/N] ");
                    io::stdout().flush()?;
                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;
                    if matches!(input.trim(), "y" | "Y") {
                        println!();
                        match core_refresh(target_num) {
                            Ok(msg) => println!("  {} {}\n", "✓".green().bold(), msg),
                            Err(e) => {
                                println!("  {} Refresh failed: {e}", "✗".red().bold());
                                println!(
                                    "  {} Switching anyway — Claude Code may reject the expired session.\n",
                                    "!".yellow().bold()
                                );
                            }
                        }
                    } else {
                        println!(
                            "  {} Switching with expired session — Claude Code may reject it.\n",
                            "!".yellow().bold()
                        );
                    }
                } else {
                    println!(
                        "  {} Switching with expired session — Claude Code may reject it.\n",
                        "!".yellow().bold()
                    );
                }
            }
        }
    }

    println!(
        "\n  {} {}  {}  {}",
        "→".cyan().bold(),
        current_slot_email.dimmed(),
        "→".dimmed(),
        target_email.cyan().bold()
    );

    core_switch(target_num)?;

    // Upgrade ~/.ccswitchrc to the new keychain-only format if needed.
    let _ = credentials::ensure_ccswitchrc();

    list()?;

    println!(
        "  {} Restart Claude Code to apply.\n",
        "✓".green().bold()
    );

    // Warn if CLAUDE_CODE_OAUTH_TOKEN is set — it overrides the keychain and
    // will cause Claude Code to ignore the switch until it is cleared.
    if std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok() {
        let rc = credentials::ccswitchrc_path();
        println!(
            "  {} {} is set in this shell.",
            "!".yellow().bold(),
            "CLAUDE_CODE_OAUTH_TOKEN".yellow().bold(),
        );
        println!(
            "  {} Run {} or {} to clear it before restarting Claude Code.\n",
            " ".normal(),
            "unset CLAUDE_CODE_OAUTH_TOKEN".cyan().bold(),
            format!("source {}", rc.display()).cyan(),
        );
    }

    Ok(())
}

// ── Credential helpers ────────────────────────────────────────────────────────

/// Check whether a token value is already stored in any managed account.
/// Returns (account_num, email) if found.
fn find_account_by_token(seq: &SequenceFile, token: &str) -> Option<(u32, String)> {
    for &num in &seq.sequence {
        let entry = match seq.accounts.get(&num.to_string()) {
            Some(e) => e,
            None => continue,
        };
        if entry.auth_kind != AuthKind::Token {
            continue;
        }
        if let Ok(creds) = credentials::read_backup(num, &entry.email) {
            if let Ok(stored) = extract_access_token(&creds) {
                if stored == token {
                    return Some((num, entry.email.clone()));
                }
            }
        }
    }
    None
}

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

// ── Session expiry helper ─────────────────────────────────────────────────────

/// Compare the fingerprint of `creds` against the stored one in `seq`.
/// Prints a warning to stderr when they differ.
fn warn_if_format_changed(seq: &SequenceFile, creds: &str) {
    let Some(stored_fp) = seq.format_fingerprint.as_deref() else {
        return;
    };
    let live_fp = credentials::credential_field_fingerprint(creds);
    if !live_fp.is_empty() && live_fp != stored_fp {
        eprintln!(
            "\n  {} Claude Code may have changed its credential format. \
             ccswitch may not work correctly. \
             Please check https://github.com/vyshnavsdeepak/ccswitch/issues for updates.",
            "Warning:".yellow().bold()
        );
    }
}

/// Return a short badge string describing session expiry for display in lists.
/// Empty string means the session is healthy and no badge is needed.
fn session_expiry_badge(creds_json: &str) -> String {
    match credentials::oauth_secs_remaining(creds_json) {
        None => String::new(),
        Some(secs) if secs <= 0 => "[expired]".to_string(),
        Some(secs) if secs <= 3 * 24 * 3600 => {
            let hours = secs / 3600;
            format!("[~{}h]", hours)
        }
        Some(secs) if secs <= 7 * 24 * 3600 => {
            let days = secs / 86400;
            format!("[~{}d]", days)
        }
        _ => String::new(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sequence::{AccountEntry, AuthKind, SequenceFile},
        test_utils::TestEnv,
    };
    use std::fs;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_oauth_creds(label: &str) -> String {
        let expires = chrono::Utc::now().timestamp_millis() + 30 * 24 * 3600 * 1000_i64;
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", label),
                "refreshToken": format!("sk-ant-ort01-{}", label),
                "expiresAt": expires
            }
        })
        .to_string()
    }

    fn make_token_backup(token: &str) -> String {
        serde_json::json!({ "token": token }).to_string()
    }

    fn make_oauth_config(email: &str, uuid: &str) -> serde_json::Value {
        serde_json::json!({
            "oauthAccount": { "emailAddress": email, "accountUuid": uuid },
            "numStartups": 1
        })
    }

    fn write_live_file(env: &TestEnv, creds: &str) {
        fs::write(env.dir.path().join(".credentials.json"), creds).unwrap();
    }

    fn write_config_file(env: &TestEnv, config: &serde_json::Value) {
        fs::write(
            env.dir.path().join(".claude.json"),
            serde_json::to_string_pretty(config).unwrap(),
        )
        .unwrap();
    }

    fn read_live_json(env: &TestEnv) -> serde_json::Value {
        let raw = fs::read_to_string(env.dir.path().join(".credentials.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn read_config_json(env: &TestEnv) -> serde_json::Value {
        let raw = fs::read_to_string(env.dir.path().join(".claude.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn entry(email: &str, kind: AuthKind) -> AccountEntry {
        AccountEntry {
            email: email.to_string(),
            uuid: format!("uuid-{email}"),
            added: sequence::now_utc(),
            auth_kind: kind,
        }
    }

    /// Two OAuth accounts, account 1 active.
    fn setup_two_oauth(env: &TestEnv) {
        let creds1 = make_oauth_creds("acct1");
        let creds2 = make_oauth_creds("acct2");
        let cfg1 = make_oauth_config("acct1@test.com", "uuid1");
        let cfg2 = make_oauth_config("acct2@test.com", "uuid2");

        let mut seq = SequenceFile::default();
        seq.accounts.insert("1".into(), entry("acct1@test.com", AuthKind::Oauth));
        seq.accounts.insert("2".into(), entry("acct2@test.com", AuthKind::Oauth));
        seq.sequence = vec![1, 2];
        seq.active_account_number = Some(1);
        seq.last_updated = sequence::now_utc();
        sequence::save(&seq).unwrap();

        write_live_file(env, &creds1);
        write_config_file(env, &cfg1);

        // Backup for account 2
        credentials::write_backup(2, "acct2@test.com", &creds2).unwrap();
        fs::write(
            config_backup_path(2, "acct2@test.com"),
            serde_json::to_string_pretty(&cfg2).unwrap(),
        )
        .unwrap();
    }

    // ── Unit tests ────────────────────────────────────────────────────────────

    #[test]
    fn test_extract_access_token_ok() {
        let creds = r#"{"token": "sk-ant-oat01-mytoken"}"#;
        assert_eq!(extract_access_token(creds).unwrap(), "sk-ant-oat01-mytoken");
    }

    #[test]
    fn test_extract_access_token_missing() {
        let creds = r#"{"claudeAiOauth": {"accessToken": "tok"}}"#;
        assert!(extract_access_token(creds).is_err());
    }

    #[test]
    fn test_session_expiry_badge_expired() {
        let ms = chrono::Utc::now().timestamp_millis() - 1_000;
        let creds = serde_json::json!({ "claudeAiOauth": { "expiresAt": ms } }).to_string();
        assert_eq!(session_expiry_badge(&creds), "[expired]");
    }

    #[test]
    fn test_session_expiry_badge_hours() {
        // Add a 60s buffer so integer division still yields 2h when the test runs.
        let ms = chrono::Utc::now().timestamp_millis() + (2 * 3600 + 60) * 1000_i64;
        let creds = serde_json::json!({ "claudeAiOauth": { "expiresAt": ms } }).to_string();
        assert_eq!(session_expiry_badge(&creds), "[~2h]");
    }

    #[test]
    fn test_session_expiry_badge_days() {
        // Add a 1h buffer so integer division still yields 4d when the test runs.
        let ms = chrono::Utc::now().timestamp_millis() + (4 * 86400 + 3600) * 1000_i64;
        let creds = serde_json::json!({ "claudeAiOauth": { "expiresAt": ms } }).to_string();
        assert_eq!(session_expiry_badge(&creds), "[~4d]");
    }

    #[test]
    fn test_session_expiry_badge_healthy() {
        let ms = chrono::Utc::now().timestamp_millis() + 30 * 86400 * 1000_i64;
        let creds = serde_json::json!({ "claudeAiOauth": { "expiresAt": ms } }).to_string();
        assert_eq!(session_expiry_badge(&creds), "");
    }

    // ── Integration tests: core_switch ────────────────────────────────────────

    #[test]
    fn test_switch_already_active() {
        let env = TestEnv::new();
        setup_two_oauth(&env);

        let msg = core_switch(1).unwrap();
        assert!(msg.contains("Already using"), "unexpected: {msg}");
        // Sequence unchanged
        assert_eq!(sequence::load().unwrap().active_account_number, Some(1));
    }

    #[test]
    fn test_switch_nonexistent_account() {
        let env = TestEnv::new();
        setup_two_oauth(&env);

        let err = core_switch(99).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "unexpected: {err}");
    }

    #[test]
    fn test_switch_oauth_to_oauth() {
        let env = TestEnv::new();
        setup_two_oauth(&env);

        let msg = core_switch(2).unwrap();
        assert!(msg.contains("acct2@test.com"), "unexpected: {msg}");

        // Sequence points at account 2
        assert_eq!(sequence::load().unwrap().active_account_number, Some(2));

        // Live credentials are account 2's token
        let live = read_live_json(&env);
        assert_eq!(
            live["claudeAiOauth"]["accessToken"].as_str().unwrap(),
            "sk-ant-oat01-acct2"
        );

        // Live config oauthAccount is account 2
        let cfg = read_config_json(&env);
        assert_eq!(
            cfg["oauthAccount"]["emailAddress"].as_str().unwrap(),
            "acct2@test.com"
        );
    }

    #[test]
    fn test_switch_oauth_to_token() {
        let env = TestEnv::new();

        let creds1 = make_oauth_creds("acct1");
        let cfg1 = make_oauth_config("acct1@test.com", "uuid1");
        let token2 = "sk-ant-oat01-tokenacct2";

        let mut seq = SequenceFile::default();
        seq.accounts.insert("1".into(), entry("acct1@test.com", AuthKind::Oauth));
        seq.accounts.insert("2".into(), entry("tokenuser", AuthKind::Token));
        seq.sequence = vec![1, 2];
        seq.active_account_number = Some(1);
        seq.last_updated = sequence::now_utc();
        sequence::save(&seq).unwrap();

        write_live_file(&env, &creds1);
        write_config_file(&env, &cfg1);
        credentials::write_backup(2, "tokenuser", &make_token_backup(token2)).unwrap();

        let msg = core_switch(2).unwrap();
        assert!(msg.contains("tokenuser"), "unexpected: {msg}");

        // Sequence points at account 2
        assert_eq!(sequence::load().unwrap().active_account_number, Some(2));

        // Live credentials contain the raw token as the OAuth accessToken
        let live = read_live_json(&env);
        assert_eq!(live["claudeAiOauth"]["accessToken"].as_str().unwrap(), token2);

        // oauthAccount is removed from config
        let cfg = read_config_json(&env);
        assert!(cfg.get("oauthAccount").is_none(), "oauthAccount should be absent");
    }

    #[test]
    fn test_switch_token_to_oauth() {
        let env = TestEnv::new();

        let token1 = "sk-ant-oat01-mytokenacct";
        let creds2 = make_oauth_creds("acct2");
        let cfg2 = make_oauth_config("acct2@test.com", "uuid2");

        let mut seq = SequenceFile::default();
        seq.accounts.insert("1".into(), entry("tokenuser", AuthKind::Token));
        seq.accounts.insert("2".into(), entry("acct2@test.com", AuthKind::Oauth));
        seq.sequence = vec![1, 2];
        seq.active_account_number = Some(1);
        seq.last_updated = sequence::now_utc();
        sequence::save(&seq).unwrap();

        // Live credentials: token stored in OAuth format (as write_live_token would do)
        let live_token_creds = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": token1,
                "refreshToken": "",
                "expiresAt": chrono::Utc::now().timestamp_millis() + 10 * 365 * 24 * 3600 * 1000_i64
            }
        })
        .to_string();
        write_live_file(&env, &live_token_creds);
        write_config_file(&env, &serde_json::json!({ "numStartups": 1 }));

        // Backup for account 2
        credentials::write_backup(2, "acct2@test.com", &creds2).unwrap();
        fs::write(
            config_backup_path(2, "acct2@test.com"),
            serde_json::to_string_pretty(&cfg2).unwrap(),
        )
        .unwrap();

        let msg = core_switch(2).unwrap();
        assert!(msg.contains("acct2@test.com"), "unexpected: {msg}");

        // Sequence points at account 2
        assert_eq!(sequence::load().unwrap().active_account_number, Some(2));

        // Live credentials are account 2's OAuth token
        let live = read_live_json(&env);
        assert_eq!(
            live["claudeAiOauth"]["accessToken"].as_str().unwrap(),
            "sk-ant-oat01-acct2"
        );

        // Live config now has account 2's oauthAccount
        let cfg = read_config_json(&env);
        assert_eq!(
            cfg["oauthAccount"]["emailAddress"].as_str().unwrap(),
            "acct2@test.com"
        );
    }

    #[test]
    fn test_switch_oauth_to_oauth_snapshots_current() {
        let env = TestEnv::new();
        setup_two_oauth(&env);

        // Switch to account 2
        core_switch(2).unwrap();

        // Now switch back to account 1 — its backup should have been written
        // when we switched away from it
        core_switch(1).unwrap();

        // Live creds should be back to account 1's token
        let live = read_live_json(&env);
        assert_eq!(
            live["claudeAiOauth"]["accessToken"].as_str().unwrap(),
            "sk-ant-oat01-acct1"
        );
        assert_eq!(sequence::load().unwrap().active_account_number, Some(1));
    }

    fn make_expired_oauth_creds(label: &str) -> String {
        let expires = chrono::Utc::now().timestamp_millis() - 3_600_000_i64; // 1h ago
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", label),
                "refreshToken": format!("sk-ant-ort01-{}", label),
                "expiresAt": expires
            }
        })
        .to_string()
    }

    /// In a non-interactive (non-tty) environment the expiry prompt is skipped
    /// and do_switch proceeds with the expired session, completing successfully.
    #[test]
    fn test_do_switch_expired_oauth_skips_prompt_and_switches() {
        let env = TestEnv::new();

        let creds1 = make_oauth_creds("acct1");
        let creds2_expired = make_expired_oauth_creds("acct2");
        let cfg1 = make_oauth_config("acct1@test.com", "uuid1");
        let cfg2 = make_oauth_config("acct2@test.com", "uuid2");

        let mut seq = SequenceFile::default();
        seq.accounts.insert("1".into(), entry("acct1@test.com", AuthKind::Oauth));
        seq.accounts.insert("2".into(), entry("acct2@test.com", AuthKind::Oauth));
        seq.sequence = vec![1, 2];
        seq.active_account_number = Some(1);
        seq.last_updated = sequence::now_utc();
        sequence::save(&seq).unwrap();

        write_live_file(&env, &creds1);
        write_config_file(&env, &cfg1);

        credentials::write_backup(2, "acct2@test.com", &creds2_expired).unwrap();
        fs::write(
            config_backup_path(2, "acct2@test.com"),
            serde_json::to_string_pretty(&cfg2).unwrap(),
        )
        .unwrap();

        // Non-tty: do_switch should warn but still switch successfully.
        do_switch(2).unwrap();

        assert_eq!(sequence::load().unwrap().active_account_number, Some(2));
        let live = read_live_json(&env);
        assert_eq!(
            live["claudeAiOauth"]["accessToken"].as_str().unwrap(),
            "sk-ant-oat01-acct2"
        );
    }

    /// Switching to a healthy (non-expired) OAuth account takes the normal path
    /// with no expiry warning.
    #[test]
    fn test_do_switch_healthy_oauth_no_expiry_warning() {
        let env = TestEnv::new();
        setup_two_oauth(&env); // both accounts have fresh creds

        // Should succeed without any expiry-related branching.
        do_switch(2).unwrap();

        assert_eq!(sequence::load().unwrap().active_account_number, Some(2));
    }

    // ── Tests: refresh --all ──────────────────────────────────────────────────

    fn make_oauth_creds_with_expiry(label: &str, expires_at_ms: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", label),
                "refreshToken": format!("sk-ant-ort01-{}", label),
                "expiresAt": expires_at_ms
            }
        })
        .to_string()
    }

    /// Build a SequenceFile with one or more accounts (does not persist to disk).
    fn seq_with_accounts(entries: &[(u32, &str, AuthKind)]) -> SequenceFile {
        let mut seq = SequenceFile::default();
        for &(num, email, ref kind) in entries {
            seq.accounts.insert(
                num.to_string(),
                AccountEntry {
                    email: email.to_string(),
                    uuid: format!("uuid-{num}"),
                    added: sequence::now_utc(),
                    auth_kind: kind.clone(),
                },
            );
            seq.sequence.push(num);
        }
        seq.active_account_number = entries.first().map(|&(num, _, _)| num);
        seq.last_updated = sequence::now_utc();
        seq
    }

    #[test]
    fn test_refresh_all_skips_token_accounts() {
        let _env = TestEnv::new();

        let seq = seq_with_accounts(&[(1, "token@test.com", AuthKind::Token)]);
        sequence::save(&seq).unwrap();

        // No credentials written — token accounts should be skipped without error.
        let result = refresh_all(&seq);
        assert!(result.is_ok(), "expected Ok for all-token seq, got: {:?}", result);
    }

    #[test]
    fn test_refresh_all_skips_healthy_oauth() {
        let _env = TestEnv::new();

        let expires_ms = chrono::Utc::now().timestamp_millis() + 30 * 86400 * 1000_i64; // 30 days
        let creds = make_oauth_creds_with_expiry("healthy", expires_ms);

        let mut seq = seq_with_accounts(&[(1, "healthy@test.com", AuthKind::Oauth)]);
        seq.active_account_number = Some(1);
        sequence::save(&seq).unwrap();

        // Write as live credentials (account 1 is active)
        fs::write(
            _env.dir.path().join(".credentials.json"),
            &creds,
        )
        .unwrap();

        let result = refresh_all(&seq);
        assert!(result.is_ok(), "healthy account should be skipped, got: {:?}", result);
    }

    #[test]
    fn test_refresh_all_attempts_expired_oauth() {
        let _env = TestEnv::new();

        let expires_ms = chrono::Utc::now().timestamp_millis() - 3600 * 1000_i64; // 1h ago
        let creds = make_oauth_creds_with_expiry("expired", expires_ms);

        let mut seq = seq_with_accounts(&[(1, "expired@test.com", AuthKind::Oauth)]);
        seq.active_account_number = Some(1);
        sequence::save(&seq).unwrap();

        fs::write(
            _env.dir.path().join(".credentials.json"),
            &creds,
        )
        .unwrap();

        // core_refresh will attempt a network call that fails in the test env.
        // We verify that refresh_all returns an error (account was attempted, not skipped).
        let result = refresh_all(&seq);
        assert!(result.is_err(), "expired account should be attempted and fail in test env");
    }

    #[test]
    fn test_refresh_all_attempts_expiring_soon_oauth() {
        let _env = TestEnv::new();

        let expires_ms = chrono::Utc::now().timestamp_millis() + 12 * 3600 * 1000_i64; // 12h
        let creds = make_oauth_creds_with_expiry("expiring", expires_ms);

        let mut seq = seq_with_accounts(&[(1, "expiring@test.com", AuthKind::Oauth)]);
        seq.active_account_number = Some(1);
        sequence::save(&seq).unwrap();

        fs::write(
            _env.dir.path().join(".credentials.json"),
            &creds,
        )
        .unwrap();

        // Token expires within 24h → should be attempted.
        let result = refresh_all(&seq);
        assert!(result.is_err(), "expiring-soon account should be attempted and fail in test env");
    }

    #[test]
    fn test_refresh_all_mixed_accounts() {
        let _env = TestEnv::new();

        let healthy_ms = chrono::Utc::now().timestamp_millis() + 30 * 86400 * 1000_i64;
        let expired_ms = chrono::Utc::now().timestamp_millis() - 3600 * 1000_i64;

        let creds_active = make_oauth_creds_with_expiry("active-healthy", healthy_ms);
        let creds_inactive = make_oauth_creds_with_expiry("inactive-expired", expired_ms);

        let mut seq = seq_with_accounts(&[
            (1, "active@test.com", AuthKind::Oauth),
            (2, "expired@test.com", AuthKind::Oauth),
            (3, "token@test.com", AuthKind::Token),
        ]);
        seq.active_account_number = Some(1);
        sequence::save(&seq).unwrap();

        // Account 1 active (healthy)
        fs::write(_env.dir.path().join(".credentials.json"), &creds_active).unwrap();
        // Account 2 backup (expired)
        credentials::write_backup(2, "expired@test.com", &creds_inactive).unwrap();

        // Account 1 is healthy → skipped. Account 3 is token → skipped.
        // Account 2 is expired → attempted → fails in test env.
        let result = refresh_all(&seq);
        assert!(result.is_err(), "one expired account should cause overall failure");
    }

    #[test]
    fn test_refresh_flag_all_with_identifier_is_error() {
        let _env = TestEnv::new();

        // Set up a minimal valid sequence so we get past the empty-check
        let seq = seq_with_accounts(&[(1, "user@test.com", AuthKind::Oauth)]);
        sequence::save(&seq).unwrap();

        let result = refresh(Some("1"), true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("--all"));
    }
}
