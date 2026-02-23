# ccswitch

Switch between multiple Claude Code accounts instantly — no logging in and out.

![demo](demo.gif)

---

## Install

```bash
cargo install --git https://github.com/vyshnavsdeepak/ccswitch
```

Or build from source:

```bash
git clone https://github.com/vyshnavsdeepak/ccswitch
cd ccswitch
cargo install --path .
```

---

## Usage

```
ccswitch              # open interactive TUI (recommended)
ccswitch add          # add current account
ccswitch list         # list managed accounts
ccswitch status       # show active account
ccswitch switch [n]   # switch to account n (or rotate to next)
ccswitch remove [n]   # remove account n
```

### TUI

Run `ccswitch` with no arguments to open the interactive account switcher.

| Key | Action |
|-----|--------|
| `↑ / k` | move up |
| `↓ / j` | move down |
| `Enter / Space` | switch to selected account |
| `a` | add current account |
| `d` | remove selected account |
| `q / Esc` | quit |

### OAuth accounts

```bash
# Log in to Claude Code, then:
ccswitch add

# Switch to account 2:
ccswitch switch 2
```

### Token accounts (`claude setup-token`)

```bash
ccswitch add
# Detects your token from $CLAUDE_CODE_OAUTH_TOKEN automatically.
# On first add, prints a one-time setup line for ~/.zshrc:
#   source ~/.ccswitchrc
```

After adding the `source` line and opening a new terminal, ccswitch manages `CLAUDE_CODE_OAUTH_TOKEN` automatically on every switch — `~/.zshrc` is never touched again.

---

## How it works

- Credentials are stored per-account in the **system keychain** (macOS) or `~/.claude-switch-backup/` with `0600` permissions (Linux/WSL).
- On each switch, ccswitch swaps the live Claude credentials and config — Claude Code sees a clean account on restart.
- For token accounts, ccswitch also updates a `ccswitch-active-token` keychain entry that `~/.ccswitchrc` reads on each new shell, so `CLAUDE_CODE_OAUTH_TOKEN` always reflects the active account.

---

## Platforms

| Platform | Credentials |
|----------|------------|
| macOS | system keychain via `security(1)` |
| Linux | `~/.claude-switch-backup/credentials/` (mode 0600) |
| WSL | same as Linux |
