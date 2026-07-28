# Demo recording

`docs/assets/demo.gif` is recorded with [vhs](https://github.com/charmbracelet/vhs)
from synthetic fixtures, never from a real session. Nothing here touches
`~/.codex`, `~/.claude`, or the real state directory.

```sh
# 1. Build an isolated demo home from the checked-in fixtures.
rm -rf /tmp/ac-demo-home /tmp/ac-demo-state
cp -R tests/fixtures/readme-demo/home /tmp/ac-demo-home
mkdir -p /tmp/ac-demo-state

# 2. Refresh transcript timestamps; discovery only shows the last seven days.
i=0
for f in /tmp/ac-demo-home/.claude/projects/demo/*.jsonl \
         /tmp/ac-demo-home/.codex/sessions/2026/07/18/*.jsonl; do
  touch -d "$(date -v-${i}M '+%Y-%m-%dT%H:%M:%S')" "$f"
  i=$((i + 7))
done

# 3. Create the demo workspaces as git repositories with pending changes.
for d in acme-web payments platform infrastructure mobile-app developer-tools docs; do
  w=/tmp/agent-console-demo/$d
  mkdir -p "$w/src" "$w/tests"
  printf '# %s\n' "$d" > "$w/README.md"
  printf 'export const version = "0.4.2";\n' > "$w/src/index.ts"
  printf 'export function handler() {}\n' > "$w/src/handler.ts"
  printf 'test("smoke", () => {});\n' > "$w/tests/smoke.test.ts"
  git -C "$w" init -q
  git -C "$w" add -A
  git -C "$w" -c user.email=demo@example.com -c user.name=demo commit -qm initial
  printf 'export function handler() { retryWithBackoff(); }\n' > "$w/src/handler.ts"
  printf 'test("smoke", () => {});\ntest("retry order", () => {});\n' > "$w/tests/smoke.test.ts"
done

# 4. Record. Replace $REPO in both files with the checkout path first.
vhs docs/demo/demo.tape
```

`inject-alerts.sh` runs alongside the recording and feeds two
`PermissionRequest` hooks through `agent-console hook`. Alerts come from state
transitions observed while the runtime is live, so a session that is already
waiting when the dashboard starts raises no alert; the events have to arrive
during the recording.
