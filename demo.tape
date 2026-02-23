Output demo.gif

Set FontSize 13
Set Width 700
Set Height 380
Set Theme "Catppuccin Mocha"
Set Shell zsh
Set Padding 20
Set TypingSpeed 60ms
Set PlaybackSpeed 0.85
Set LoopOffset 5%

# vhs spawns a non-login zsh — inject ~/.cargo/bin so ccswitch is found
Env PATH "/Users/vyshnav/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"

# Open TUI
Type "ccswitch"
Enter
Sleep 2s                    # let TUI fully render

# Navigate to Account 2
Down
Sleep 700ms

# Initiate switch — triggers ConfirmSwitch dialog
Enter
Sleep 800ms                 # let dialog render

# Confirm — REQUIRED by tui.rs ConfirmSwitch handler
Type "y"
Sleep 1.5s                  # let Done screen render

# Dismiss Done screen (any key)
Enter
Sleep 1s                    # pause before re-opening

# Re-open TUI to show Account 2 is now active, switch back
Type "ccswitch"
Enter
Sleep 2s

Up                          # navigate back to Account 1
Sleep 700ms

Enter                       # ConfirmSwitch dialog
Sleep 800ms

Type "y"                    # confirm
Sleep 1.5s                  # Done screen

Enter                       # dismiss
Sleep 1s
