#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="$repo_root/.github/workflows/release.yml"

fail() {
  echo "release workflow check failed: $*" >&2
  exit 1
}

dmg_matrix_entries="$(grep -c 'archive: dmg' "$workflow" || true)"
[[ "$dmg_matrix_entries" == "2" ]] \
  || fail "expected two macOS DMG matrix entries, found $dmg_matrix_entries"

grep -Fq 'xcrun notarytool submit "$dmg"' "$workflow" \
  || fail "the final DMG is not submitted to Apple notarization"
grep -Fq 'xcrun stapler staple "$dmg"' "$workflow" \
  || fail "the notarization ticket is not stapled to the final DMG"
grep -Fq 'xcrun stapler validate "$dmg"' "$workflow" \
  || fail "the stapled ticket is not validated"
grep -Fq 'context:primary-signature "$dmg"' "$workflow" \
  || fail "the final DMG is not assessed by Gatekeeper"
grep -Fq 'Notarization Ticket=stapled' "$workflow" \
  || fail "the workflow does not assert that the public DMG carries its ticket"

if awk '
  /target: (x86_64|aarch64)-apple-darwin/ { mac = 1; next }
  mac && /archive: tar\.gz/ { bad = 1 }
  mac && /archive:/ { mac = 0 }
  END { exit bad ? 0 : 1 }
' "$workflow"; then
  fail "macOS still publishes tar.gz instead of a stapled container"
fi

echo "release workflow check passed"
