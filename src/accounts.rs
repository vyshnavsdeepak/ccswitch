use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::{
    io::{self, Write},
    path::PathBuf,
};

use crate::{
    config, credentials,
    sequence::{self, AccountEntry, now_utc},
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

    let target_email = seq
        .accounts
        .get(&target_num.to_string())
        .map(|e| e.email.clone())
        .with_context(|| format!("Account {target_num} does not exist"))?;

    let current_email =
        config::current_email().context("No active Claude account found")?;

    let current_num = seq
        .active_account_number
        .or_else(|| seq.find_by_email(&current_email))
        .with_context(|| {
            format!("Cannot determine account slot for '{current_email}'. Run `ccswitch add` first.")
        })?;

    if target_num == current_num {
        return Ok(format!(
            "Already using {} (Account {}).",
            target_email, target_num
        ));
    }

    let current_slot_email = seq
        .accounts
        .get(&current_num.to_string())
        .map(|e| e.email.clone())
        .unwrap_or_else(|| current_email.clone());

    // Step 1: Snapshot current account
    let live_creds = credentials::read_live().context("Cannot read current credentials")?;
    let live_config = config::load().context("Cannot read current Claude config")?;
    let live_config_str = serde_json::to_string_pretty(&live_config)?;

    credentials::write_backup(current_num, &current_slot_email, &live_creds)?;
    write_config_backup(current_num, &current_slot_email, &live_config_str)?;

    // Step 2: Restore target account
    let target_creds = credentials::read_backup(target_num, &target_email)
        .with_context(|| format!("Missing credentials backup for Account {target_num}"))?;

    let target_config_str = read_config_backup(target_num, &target_email)
        .with_context(|| format!("Missing config backup for Account {target_num}"))?;

    let target_config: serde_json::Value = serde_json::from_str(&target_config_str)
        .context("Invalid JSON in config backup")?;

    let target_oauth = target_config
        .get("oauthAccount")
        .cloned()
        .context("Missing oauthAccount in config backup")?;

    // Step 3: Activate
    credentials::write_live(&target_creds).context("Failed to write credentials")?;

    let mut active_config = config::load().context("Cannot read live config for merge")?;
    active_config["oauthAccount"] = target_oauth;
    config::save(&active_config).context("Failed to save merged config")?;

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

    let current_email = config::current_email();
    let active_num = current_email.as_deref().and_then(|e| seq.find_by_email(e));

    println!("\n  {}", "Managed Accounts".bold());
    println!("  {}", "─".repeat(40).dimmed());

    for &num in &seq.sequence {
        let Some(entry) = seq.accounts.get(&num.to_string()) else {
            continue;
        };

        if active_num == Some(num) {
            println!(
                "  {}  {}  {}",
                format!("▶ {num:>2}").green().bold(),
                entry.email.green().bold(),
                "(active)".green().dimmed()
            );
        } else {
            println!(
                "  {}  {}",
                format!("  {num:>2}").dimmed(),
                entry.email
            );
        }
    }

    println!("  {}\n", "─".repeat(40).dimmed());
    Ok(())
}

// ── Status ────────────────────────────────────────────────────────────────────

pub fn status() -> Result<()> {
    let seq = sequence::load()?;

    match config::current_email() {
        None => {
            println!("\n  {} Not logged in to Claude Code.\n", "✗".red().bold());
        }
        Some(email) => {
            let account_num = seq.find_by_email(&email);
            match account_num {
                Some(num) => println!(
                    "\n  {} {} {}\n",
                    "▶".green().bold(),
                    email.bold(),
                    format!("(Account {num})").dimmed()
                ),
                None => println!(
                    "\n  {} {} {}\n",
                    "▶".yellow().bold(),
                    email.bold(),
                    "(not managed — run `ccswitch add`)".dimmed()
                ),
            }
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

    if seq.sequence.len() < 2 {
        bail!("Only one account managed. Add another with `ccswitch add`.");
    }

    let active_num = seq
        .active_account_number
        .context("No active account number in state file")?;

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
    let target_email = seq
        .accounts
        .get(&target_num.to_string())
        .map(|e| e.email.clone())
        .with_context(|| format!("Account {target_num} does not exist"))?;

    let current_email = config::current_email().context("No active Claude account found")?;
    let current_num = seq
        .active_account_number
        .or_else(|| seq.find_by_email(&current_email))
        .with_context(|| {
            format!("Cannot determine account slot for '{current_email}'. Run `ccswitch add` first.")
        })?;

    if target_num == current_num {
        println!(
            "\n  {} Already using {} (Account {target_num}).\n",
            "·".cyan(),
            target_email.bold()
        );
        return Ok(());
    }

    let current_slot_email = seq
        .accounts
        .get(&current_num.to_string())
        .map(|e| e.email.clone())
        .unwrap_or_else(|| current_email.clone());

    println!(
        "\n  {} {}  {}  {}",
        "→".cyan().bold(),
        current_slot_email.dimmed(),
        "→".dimmed(),
        target_email.cyan().bold()
    );

    let msg = core_switch(target_num)?;

    list()?;

    println!("  {} {}\n", "→".cyan().bold(), msg.split(". ").nth(1).unwrap_or("Restart Claude Code to apply."));

    Ok(())
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
