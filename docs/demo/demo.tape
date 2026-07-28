Output "$REPO/docs/assets/demo.gif"
Set Shell zsh
Set FontSize 15
Set Width 1400
Set Height 800
Set Padding 12
Set Framerate 24

Hide
Type "export HOME=/tmp/ac-demo-home AGENT_CONSOLE_STATE_DIR=/tmp/ac-demo-state AGENT_CONSOLE_SUMMARIZER=off" Enter
Type "alias agent-console=$REPO/target/debug/agent-console" Enter
Type "/tmp/ac-inject.sh >/dev/null 2>&1 &" Enter
Type "clear" Enter
Sleep 800ms
Show

Type "agent-console" Sleep 350ms Enter
Sleep 8s

Type "a"
Sleep 3500ms

Sleep 4s
Type "a"
Sleep 3500ms

Type "/"
Sleep 500ms
Type "payments"
Sleep 2200ms
Escape
Sleep 1s

Type "s"
Sleep 1500ms
Type "git status --short" Sleep 250ms Enter
Sleep 2800ms
Ctrl+q
Sleep 2200ms

Hide
Type "q"
Sleep 800ms
