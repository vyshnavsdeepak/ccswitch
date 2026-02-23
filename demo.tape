Output demo.gif

Set Shell "zsh"
Set FontSize 15
Set Width 700
Set Height 400
Set Theme "Dracula"
Set Padding 24
Set TypingSpeed 70ms
Set PlaybackSpeed 0.85

Env PATH "/Users/vyshnav/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
Env PROMPT "%F{magenta}❯%f "

Sleep 800ms

# ── list accounts ─────────────────────────────────────────────────────────────
Type "ccswitch list"
Sleep 500ms
Enter
Sleep 2.2s

# ── status ────────────────────────────────────────────────────────────────────
Type "ccswitch status"
Sleep 500ms
Enter
Sleep 2s

# ── open TUI ──────────────────────────────────────────────────────────────────
Type "ccswitch"
Sleep 500ms
Enter
Sleep 1.8s

# navigate the list
Down
Sleep 800ms
Up
Sleep 600ms

# press enter to start a switch, then cancel
Enter
Sleep 1s
Escape
Sleep 800ms

# quit
Ctrl+c
Sleep 800ms
