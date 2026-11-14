#!/usr/bin/env bash
# Checks that an installed Annotator keeps its TCC grants across rebuilds: the app and its helper
# are signed with the pinned certificate from ~/.config/annotator/signing.json, and each
# designated requirement names only the signing identifier and that certificate leaf (no cdhash,
# which changes on every build). Prints each requirement; exits 1 on the first mismatch.
#   scripts/check-signing.sh /Applications/Annotator.app
#   scripts/check-signing.sh "$HOME/Applications/Annotator Dev.app"
set -euo pipefail
export LC_ALL=C

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/check-signing.sh <app>" >&2
  exit 2
fi
APP_DIR="${1%/}"
SIGNING_CONFIG="${HOME}/.config/annotator/signing.json"
IDENTITY="$(/usr/bin/plutil -extract identity raw -o - "${SIGNING_CONFIG}")"
BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "${APP_DIR}/Contents/Info.plist")"
status=0

check() {
  local path="$1" id="$2" requirement signed_id
  requirement="$(codesign -d -r- "${path}" 2>&1 | sed -n 's/^#* *designated => //p')"
  signed_id="$(codesign -dv "${path}" 2>&1 | sed -n 's/^Identifier=//p')"
  local want="identifier \"${id}\" and certificate leaf = H\"${IDENTITY}\""
  printf '%s\n  identifier: %s\n  designated: %s\n' "${path}" "${signed_id}" "${requirement}"
  if [[ "${signed_id}" != "${id}" ]]; then
    echo "  FAIL: signed as ${signed_id}, want ${id}" >&2
    status=1
  elif [[ "${requirement}" == *cdhash* ]]; then
    echo "  FAIL: the requirement pins a cdhash, which changes on every build" >&2
    status=1
  elif [[ "$(tr '[:upper:]' '[:lower:]' <<<"${requirement}")" != "$(tr '[:upper:]' '[:lower:]' <<<"${want}")" ]]; then
    echo "  FAIL: want ${want}" >&2
    status=1
  else
    echo "  ok"
  fi
}

codesign --verify --strict "${APP_DIR}"
check "${APP_DIR}" "${BUNDLE_ID}"
check "${APP_DIR}/Contents/MacOS/scrawl-helper" "${BUNDLE_ID}.helper"
exit "${status}"
