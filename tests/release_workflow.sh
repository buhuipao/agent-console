#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="$repo_root/.github/workflows/release.yml"

fail() {
  echo "release workflow check failed: $*" >&2
  exit 1
}

mac_tar_entries="$({
  awk '
    /target: (x86_64|aarch64)-apple-darwin/ { mac = 1; next }
    mac && /archive: tar\.gz/ { count += 1; mac = 0; next }
    mac && /archive:/ { mac = 0 }
    END { print count + 0 }
  ' "$workflow"
})"
[[ "$mac_tar_entries" == "2" ]] \
  || fail "expected two macOS tar.gz matrix entries, found $mac_tar_entries"

grep -Fq 'submission="$RUNNER_TEMP/agent-console-${TARGET}-notarization.zip"' "$workflow" \
  || fail "the signed macOS binary is not staged for notarization"
grep -Fq 'xcrun notarytool submit "$submission"' "$workflow" \
  || fail "the signed macOS binary is not submitted to Apple notarization"
grep -Fq 'if [[ "$notary_status" != "Accepted" ]]' "$workflow" \
  || fail "publishing does not require an Accepted notarization result"
grep -Fq 'codesign --verify --strict --verbose=2 "$binary"' "$workflow" \
  || fail "the signed macOS binary is not verified"
grep -Fq 'name: Verify packaged macOS binary' "$workflow" \
  || fail "the final macOS archive is not verified after extraction"
grep -Fq 'codesign --verify --strict --verbose=4 "$packaged"' "$workflow" \
  || fail "the extracted macOS release binary is not signature-verified"
grep -Fq 'install -m 755 "$packaged" "$install_dir/agent-console.new"' "$workflow" \
  || fail "the macOS atomic upgrade path is not exercised"
grep -Fq 'mv -f "$install_dir/agent-console.new" "$install_dir/agent-console"' "$workflow" \
  || fail "the macOS upgrade probe does not replace the inode atomically"
grep -Fq '"$packaged" --version | grep -Fx "agent-console ${VERSION}"' "$workflow" \
  || fail "the packaged CLI version path is not smoke-tested"

if grep -Eq 'archive: dmg|hdiutil|stapler|Notarization Ticket=stapled' "$workflow"; then
  fail "macOS release policy still contains DMG/stapling steps"
fi

echo "release workflow check passed"
