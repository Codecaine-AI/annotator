#!/usr/bin/env bash
# Installs Annotator Dev.app (~/Applications, ai.codecaine.annotator.dev) beside the live app.
set -euo pipefail
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/install-app.sh" --dev "$@"
