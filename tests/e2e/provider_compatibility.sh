#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
binary="$root/target/debug/agent-console"
config="$root/tests/fixtures/compat-config.toml"
fixture_home="$root/tests/fixtures/e2e-home"

for codex_version in 0.100.0 0.144.5; do
  for claude_version in 2.0.0 2.1.214; do
    state=$(mktemp -d /tmp/agent-console-compat.XXXXXX)
    output=$(
      cd "$root"
      AGENT_CONSOLE_CONFIG="$config" \
      AGENT_CONSOLE_STATE_DIR="$state" \
      CODEX_FIXTURE_VERSION="$codex_version" \
      CLAUDE_FIXTURE_VERSION="$claude_version" \
      HOME="$fixture_home" \
      PATH="$root/tests/fixtures/bin:$PATH" \
      "$binary" doctor
    )
    rm -rf "$state"
    printf '%s\n' "$output" | grep -q 'ok   codex version: supported'
    printf '%s\n' "$output" | grep -q 'ok   claude version: supported'
    printf 'ok compatibility Codex %s / Claude %s\n' "$codex_version" "$claude_version"
  done
done
