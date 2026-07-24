#!/bin/zsh

set -eu

typeset -a forwarded
while (( $# > 0 )); do
    if [[ "$1" == "-c" && "${2:-}" == hooks.* ]]; then
        shift 2
    else
        forwarded+=("$1")
        shift
    fi
done

exec codex "${forwarded[@]}"
