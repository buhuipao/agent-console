#!/bin/sh
AC=$REPO/target/debug/agent-console
export HOME=/tmp/ac-demo-home AGENT_CONSOLE_STATE_DIR=/tmp/ac-demo-state
sleep 7
printf '%s\n' '{"session_id":"c0000003-0000-4000-8000-000000000003","hook_event_name":"PermissionRequest","request_id":"r1","tool_name":"Edit","message":"Replace the session signing key rotation logic?"}' | "$AC" hook codex
sleep 7
printf '%s\n' '{"session_id":"55555555-5555-4555-8555-555555555555","hook_event_name":"PermissionRequest","request_id":"r2","tool_name":"Bash","message":"Run the destructive offline-sync migration?"}' | "$AC" hook claude
