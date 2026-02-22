use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph,
    },
    Terminal,
};
use std::io;

use crate::{accounts, config, sequence};
use crate::sequence::AuthKind;

// ── State machine ─────────────────────────────────────────────────────────────

enum Mode {
    Normal,
    ConfirmSwitch { num: u32, email: String },
    ConfirmRemove { num: u32, email: String },
    ConfirmAdd { email: String },
    /// Switch (or other action) completed.
    /// `needs_new_shell`: true when the active account is a token account —
    /// the user must open a new shell for CLAUDE_CODE_OAUTH_TOKEN to update.
    Done { needs_new_shell: bool },
}

struct Flash {
    message: String,
    is_error: bool,
}

struct App {
    seq: sequence::SequenceFile,
    /// Display email: prefers OAuth config, falls back to seq.active_account_number
    /// so token users also see their active account in the header.
    current_email: Option<String>,
    selected: usize,
    mode: Mode,
    flash: Option<Flash>,
    quit: bool,
}

impl App {
    fn new() -> Result<Self> {
        let seq = sequence::load()?;
        let current_email = Self::resolve_display_email(&seq);
        Ok(App {
            seq,
            current_email,
            selected: 0,
            mode: Mode::Normal,
            flash: None,
            quit: false,
        })
    }

    fn reload(&mut self) -> Result<()> {
        self.seq = sequence::load()?;
        self.current_email = Self::resolve_display_email(&self.seq);
        // clamp selection
        if !self.seq.sequence.is_empty() && self.selected >= self.seq.sequence.len() {
            self.selected = self.seq.sequence.len() - 1;
        }
        Ok(())
    }

    /// Determine the active email for display.
    /// OAuth users: read from live Claude config.
    /// Token users: fall back to the seq state (no oauthAccount in config).
    fn resolve_display_email(seq: &sequence::SequenceFile) -> Option<String> {
        config::current_email().or_else(|| {
            seq.active_account_number
                .and_then(|num| seq.accounts.get(&num.to_string()))
                .map(|e| e.email.clone())
        })
    }

    fn selected_num(&self) -> Option<u32> {
        self.seq.sequence.get(self.selected).copied()
    }

    fn active_num(&self) -> Option<u32> {
        // Prefer seq state (works for token accounts that have no oauthAccount)
        self.seq.active_account_number.or_else(|| {
            self.current_email
                .as_deref()
                .and_then(|e| self.seq.find_by_email(e))
        })
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal);

    // Always restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    terminal.show_cursor()?;

    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new()?;

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        if app.quit {
            break;
        }

        if !event::poll(std::time::Duration::from_millis(250))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            // Ctrl+C always quits
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                break;
            }

            match &app.mode {
                Mode::Normal => handle_normal(&mut app, key.code)?,
                Mode::ConfirmSwitch { .. }
                | Mode::ConfirmRemove { .. }
                | Mode::ConfirmAdd { .. } => handle_confirm(&mut app, key.code)?,
                Mode::Done { .. } => {
                    app.quit = true;
                }
            }
        }
    }
    Ok(())
}

// ── Key handlers ──────────────────────────────────────────────────────────────

fn handle_normal(app: &mut App, key: KeyCode) -> Result<()> {
    match key {
        KeyCode::Up | KeyCode::Char('k') => {
            if app.selected > 0 {
                app.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.selected + 1 < app.seq.sequence.len() {
                app.selected += 1;
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(num) = app.selected_num() {
                if let Some(entry) = app.seq.accounts.get(&num.to_string()) {
                    if app.active_num() == Some(num) {
                        app.flash = Some(Flash {
                            message: "Already the active account".to_string(),
                            is_error: false,
                        });
                    } else {
                        app.mode = Mode::ConfirmSwitch {
                            num,
                            email: entry.email.clone(),
                        };
                    }
                }
            }
        }
        KeyCode::Char('a') => {
            if let Some(ref email) = app.current_email.clone() {
                if app.seq.account_exists(email) {
                    app.flash = Some(Flash {
                        message: format!("{} is already managed", email),
                        is_error: false,
                    });
                } else {
                    app.mode = Mode::ConfirmAdd {
                        email: email.clone(),
                    };
                }
            } else {
                // No oauthAccount detected. Token users must use the CLI.
                let msg = if config::has_env_token() {
                    "Token accounts: run  ccswitch add  in a terminal to set up".to_string()
                } else {
                    "No active Claude account found — log in to Claude Code first".to_string()
                };
                app.flash = Some(Flash {
                    message: msg,
                    is_error: !config::has_env_token(),
                });
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if let Some(num) = app.selected_num() {
                if let Some(entry) = app.seq.accounts.get(&num.to_string()) {
                    app.mode = Mode::ConfirmRemove {
                        num,
                        email: entry.email.clone(),
                    };
                }
            }
        }
        KeyCode::Char('q') | KeyCode::Esc => {
            app.quit = true;
        }
        _ => {}
    }
    Ok(())
}

fn handle_confirm(app: &mut App, key: KeyCode) -> Result<()> {
    match key {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            // Clone the mode data out before mutating app
            let mode = std::mem::replace(&mut app.mode, Mode::Normal);
            match mode {
                Mode::ConfirmSwitch { num, email } => {
                    match accounts::core_switch(num) {
                        Ok(_) => {
                            app.reload()?;
                            // Token accounts require a new shell for the env var to update
                            let needs_new_shell = app
                                .seq
                                .accounts
                                .get(&num.to_string())
                                .map(|e| e.auth_kind == AuthKind::Token)
                                .unwrap_or(false);
                            app.mode = Mode::Done { needs_new_shell };
                        }
                        Err(e) => {
                            app.flash = Some(Flash {
                                message: format!("Switch failed: {}", e),
                                is_error: true,
                            });
                            let _ = email; // suppress warning
                        }
                    }
                }
                Mode::ConfirmRemove { num, email } => {
                    match accounts::core_remove(num, &email) {
                        Ok(_) => {
                            app.reload()?;
                            app.flash = Some(Flash {
                                message: format!("Removed Account {} ({})", num, email),
                                is_error: false,
                            });
                        }
                        Err(e) => {
                            app.flash = Some(Flash {
                                message: format!("Remove failed: {}", e),
                                is_error: true,
                            });
                        }
                    }
                }
                Mode::ConfirmAdd { email } => {
                    match accounts::core_add() {
                        Ok(msg) => {
                            app.reload()?;
                            app.flash = Some(Flash {
                                message: msg,
                                is_error: false,
                            });
                            let _ = email;
                        }
                        Err(e) => {
                            app.flash = Some(Flash {
                                message: format!("Add failed: {}", e),
                                is_error: true,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.flash = Some(Flash {
                message: "Cancelled".to_string(),
                is_error: false,
            });
        }
        _ => {}
    }
    Ok(())
}

// ── UI rendering ──────────────────────────────────────────────────────────────

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(3),    // account list
            Constraint::Length(3), // help bar
        ])
        .split(area);

    render_header(f, app, chunks[0]);
    render_list(f, app, chunks[1]);
    render_help(f, app, chunks[2]);

    // Overlay confirmation dialog if needed
    match &app.mode {
        Mode::ConfirmSwitch { num, email } => {
            render_confirm_dialog(
                f,
                area,
                "Switch Account",
                &format!("Switch to Account {}?", num),
                email,
                Color::Yellow,
            );
        }
        Mode::ConfirmRemove { num, email } => {
            render_confirm_dialog(
                f,
                area,
                "Remove Account",
                &format!("Remove Account {}?", num),
                email,
                Color::Red,
            );
        }
        Mode::ConfirmAdd { email } => {
            render_confirm_dialog(
                f,
                area,
                "Add Account",
                "Add current account?",
                email,
                Color::Yellow,
            );
        }
        _ => {}
    }
}

fn render_header(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let email_text = app
        .current_email
        .as_deref()
        .unwrap_or("not logged in");

    let block = Block::default()
        .title(" ccswitch ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let text = Paragraph::new(Line::from(vec![
        Span::styled("  Active: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            email_text,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(block);

    f.render_widget(text, area);
}

fn render_list(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let account_count = app.seq.sequence.len();
    let title = if account_count == 1 {
        " 1 account ".to_string()
    } else {
        format!(" {} accounts ", account_count)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    if app.seq.sequence.is_empty() {
        let text = Paragraph::new(Line::from(vec![Span::styled(
            "  No accounts managed yet. Press [a] to add the current account.",
            Style::default().fg(Color::DarkGray),
        )]))
        .block(block);
        f.render_widget(text, area);
        return;
    }

    let active_num = app.active_num();

    let items: Vec<ListItem> = app
        .seq
        .sequence
        .iter()
        .map(|&num| {
            let entry = match app.seq.accounts.get(&num.to_string()) {
                Some(e) => e,
                None => return ListItem::new(""),
            };

            let is_active = active_num == Some(num);
            let is_token = entry.auth_kind == AuthKind::Token;

            if is_active {
                let mut spans = vec![
                    Span::styled(
                        format!("  ▶  {:>2}  ", num),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        entry.email.clone(),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                if is_token {
                    spans.push(Span::styled(
                        "  [token]",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::DIM),
                    ));
                }
                spans.push(Span::styled(
                    "  active",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::DIM),
                ));
                ListItem::new(Line::from(spans))
            } else {
                let mut spans = vec![
                    Span::styled(
                        format!("     {:>2}  ", num),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(entry.email.clone(), Style::default().fg(Color::White)),
                ];
                if is_token {
                    spans.push(Span::styled(
                        "  [token]",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 60))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");

    let mut list_state = ListState::default();
    list_state.select(Some(app.selected));

    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_help(f: &mut ratatui::Frame, app: &App, area: Rect) {
    match &app.mode {
        Mode::Done { needs_new_shell } => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Green));

            let mut spans = vec![
                Span::styled(
                    "  ✓ Done  ·  ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Restart Claude Code",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ];

            if *needs_new_shell {
                spans.push(Span::styled(
                    "  ·  open a new shell",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            spans.push(Span::styled(
                "  ·  [any key] quit",
                Style::default().fg(Color::Green),
            ));

            let text = Paragraph::new(Line::from(spans)).block(block);
            f.render_widget(text, area);
        }
        _ => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray));

            let content = if let Some(flash) = &app.flash {
                let color = if flash.is_error {
                    Color::Red
                } else {
                    Color::Green
                };
                let icon = if flash.is_error { "✗" } else { "✓" };
                Line::from(vec![
                    Span::styled(
                        format!("  {}  ", icon),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(flash.message.clone(), Style::default().fg(color)),
                ])
            } else {
                Line::from(vec![Span::styled(
                    "  ↑↓ nav  ·  ↵ switch  ·  a add  ·  d remove  ·  q quit",
                    Style::default().fg(Color::DarkGray),
                )])
            };

            let text = Paragraph::new(content).block(block);
            f.render_widget(text, area);
        }
    }
}

fn render_confirm_dialog(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    action_line: &str,
    email: &str,
    border_color: Color,
) {
    let dialog_width = 54u16;
    let dialog_height = 7u16;

    let x = area.x + area.width.saturating_sub(dialog_width) / 2;
    let y = area.y + area.height.saturating_sub(dialog_height) / 2;

    let dialog_area = Rect {
        x,
        y,
        width: dialog_width.min(area.width),
        height: dialog_height.min(area.height),
    };

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(format!(" {} ", title))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let text = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("   {}", action_line),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            format!("   {}", email),
            Style::default().fg(Color::Yellow),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "   [y] confirm",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "      [n / Esc] cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];

    let para = Paragraph::new(text).alignment(Alignment::Left);
    f.render_widget(para, inner);
}
