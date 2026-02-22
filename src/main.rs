mod accounts;
mod config;
mod credentials;
mod platform;
mod sequence;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::Colorize;

#[derive(Parser)]
#[command(
    name = "ccswitch",
    version,
    about = "Multi-account switcher for Claude Code",
    long_about = "\
Manage and rotate between multiple Claude Code accounts without \
logging in and out each time.\n\
\n\
Run without arguments to open the interactive TUI.\n\
\n\
Accounts are stored in ~/.claude-switch-backup with credentials \
kept in the system keychain (macOS) or encrypted files (Linux/WSL)."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Add the currently logged-in Claude account to managed accounts
    Add,

    /// Remove a managed account by number or email
    Remove {
        /// Account number (e.g. 2) or email address
        account: String,
    },

    /// List all managed accounts
    #[command(alias = "ls")]
    List,

    /// Show the currently active account
    Status,

    /// Switch accounts — rotates to next if no argument given
    Switch {
        /// Account number or email to switch to (optional; rotates if omitted)
        account: Option<String>,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\n  {} {}\n", "Error:".red().bold(), e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    if platform::is_root() && !platform::is_container() {
        anyhow::bail!("Do not run as root (unless inside a container)");
    }

    let cli = Cli::parse();

    match cli.command {
        None => tui::run(),
        Some(Commands::Add) => accounts::add(),
        Some(Commands::Remove { account }) => accounts::remove(&account),
        Some(Commands::List) => accounts::list(),
        Some(Commands::Status) => accounts::status(),
        Some(Commands::Switch { account: None }) => accounts::switch_next(),
        Some(Commands::Switch { account: Some(id) }) => accounts::switch_to(&id),
    }
}
