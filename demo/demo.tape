# The README demo: agents sort themselves by who needs you, then one gets
# unblocked. Record from the repo root after `cargo build --release`:
#
#   vhs demo/demo.tape
#   CORRAL_SOCKET=/tmp/corral-demo/t/demo.sock corral kill-server
#
# setup.sh gives the recording a corral server of its own, so this never
# touches a real session.

Output docs/demo.gif

Set Shell bash
Set FontFamily "Menlo"
Set FontSize 15
Set Width 1440
Set Height 780
Set Padding 18
Set Framerate 24
Set TypingSpeed 60ms
Set Theme { "name": "corral", "background": "#111213", "foreground": "#e4e1e9", "cursor": "#caea72", "selection": "#2a2c2e", "black": "#1b1c1d", "red": "#f2876b", "green": "#a8d98c", "yellow": "#caea72", "blue": "#75b4f1", "magenta": "#b9aee6", "cyan": "#7fd1c7", "white": "#d9d5e1", "brightBlack": "#5a5c60", "brightRed": "#f6a08a", "brightGreen": "#bde6a6", "brightYellow": "#d8f08f", "brightBlue": "#94c5f5", "brightMagenta": "#cdc4ee", "brightCyan": "#9fdfd7", "brightWhite": "#f0edf4" }

Hide
Type "source demo/setup.sh && clear"
Enter
Sleep 500ms
Type "corral"
Enter
Sleep 1500ms
Show

# Three agents at work. docs finishes, then fixer stops to ask permission,
# and both rise into NEEDS YOU.
Sleep 11s

# ^b b jumps to the blocked agent; 1 approves and it goes back to work.
Ctrl+B
Type "b"
Sleep 2.5s
Type "1"
Sleep 4s
