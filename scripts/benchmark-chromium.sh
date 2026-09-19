#!/usr/bin/env bash
# The old count-only comparison mixed file scopes and used the user's daemon.
# Use the isolated, exact-result harness. Run with --help for required paths.
set -euo pipefail
exec python3 "$(dirname "$0")/benchmark-chromium.py" "$@"
