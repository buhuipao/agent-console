#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
state=$(mktemp -d /tmp/agent-console-real-doctor.XXXXXX)
trap 'rm -rf "$state"' EXIT INT TERM

AGENT_CONSOLE_STATE_DIR="$state" "$root/target/debug/agent-console" doctor
