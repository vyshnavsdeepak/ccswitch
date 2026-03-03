mod accounts;
mod config;
mod credentials;
mod platform;
mod sequence;
mod tui;

#[cfg(test)]
pub(crate) mod test_utils {
    use std::sync::Mutex;

    pub static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub struct TestEnv {
        pub dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestEnv {
        pub fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::TempDir::new().unwrap();
            std::env::set_var("CCSWITCH_TEST_DIR", dir.path().to_str().unwrap());
            std::env::set_var("CCSWITCH_TEST_PLATFORM", "linux");
            std::fs::create_dir_all(dir.path().join("configs")).unwrap();
            std::fs::create_dir_all(dir.path().join("credentials")).unwrap();
            TestEnv { dir, _lock: lock }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            std::env::remove_var("CCSWITCH_TEST_DIR");
            std::env::remove_var("CCSWITCH_TEST_PLATFORM");
        }
    }
}

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
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

    /// Refresh the OAuth session token for an account (active account if none given)
    Refresh {
        /// Account number or email to refresh (optional; uses active account if omitted)
        account: Option<String>,
        /// Refresh all OAuth accounts that are expired or expire within 24 h
        #[arg(long)]
        all: bool,
    },

    /// Set a short alias for an account
    Alias {
        /// Account number or email to alias
        account: String,
        /// Short name to use as the alias (e.g. "work", "personal")
        name: String,
    },

    /// Generate shell completion script
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\n  {} {}", "Error:".red().bold(), e);
        // Print cause chain if present
        let mut source = e.source();
        while let Some(cause) = source {
            eprintln!("    {} {}", "caused by:".dimmed(), cause);
            source = cause.source();
        }
        eprintln!();
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
        Some(Commands::Refresh { account, all }) => {
            accounts::refresh(account.as_deref(), all)
        }
        Some(Commands::Alias { account, name }) => accounts::set_alias(&account, &name),
        Some(Commands::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "ccswitch", &mut std::io::stdout());
            Ok(())
        }
    }
}
