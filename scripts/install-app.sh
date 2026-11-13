#!/usr/bin/env bash
set -euo pipefail
# pgrep/pkill fail under some locales (C.UTF-8), which left the old app running.
export LC_ALL=C

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ $# -gt 1 || ( $# -eq 1 && "$1" != --dev ) ]]; then
  echo "usage: scripts/install-app.sh [--dev]" >&2
  exit 2
fi
# --dev installs Annotator Dev.app beside the live app. It has its own bundle id, so its own TCC
# grants, and the app derives its own config, captures, logs, shortcuts and port from that id
# (src/app_identity.rs). It never quits, replaces or relinks the live Annotator.app.
if [[ "${1:-}" == --dev ]]; then
  APP_NAME="Annotator Dev"
  BUNDLE_ID="ai.codecaine.annotator.dev"
  # Outside target/, which `hivemind clean` purges.
  DEFAULT_APP_DIR="${HOME}/Applications/Annotator Dev.app"
  CLI_LINKS=("${HOME}/bin/scrawl-dev")
  # Only this app is quit. Annotator.app, Scrawl.app and their shortcuts are separate.
  APP_BINARIES=("Annotator Dev.app/Contents/MacOS/scrawl")
else
  APP_NAME="Annotator"
  BUNDLE_ID="ai.codecaine.annotator"
  DEFAULT_APP_DIR="/Applications/Annotator.app"
  CLI_LINKS=("${HOME}/bin/scrawl" /usr/local/bin/scrawl)
  # The upstream Scrawl.app and Annotator.app share the shortcut and the captures folder, so only
  # one may run. Scrawl.app itself stays installed.
  APP_BINARIES=("Scrawl.app/Contents/MacOS/scrawl" "Annotator.app/Contents/MacOS/scrawl")
fi
# SCRAWL_APP_DIR builds the signed bundle somewhere else (staging for remote installs, see
# install-app-remote.sh); SCRAWL_NO_LAUNCH=1 skips the CLI links and the restart.
APP_DIR="${SCRAWL_APP_DIR:-${DEFAULT_APP_DIR}}"
CONTENTS_DIR="${APP_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"
FRAMEWORKS_DIR="${CONTENTS_DIR}/Frameworks"
HELPER_ID="${BUNDLE_ID}.helper"
# The pinned local identity from scripts/setup-local-signing.py. Signing with the same
# certificate and an explicit designated requirement keeps TCC grants across rebuilds.
SIGNING_CONFIG="${HOME}/.config/annotator/signing.json"

if [[ ! -f "${SIGNING_CONFIG}" ]]; then
  echo "no ${SIGNING_CONFIG}; run python3 scripts/setup-local-signing.py first" >&2
  exit 1
fi
IDENTITY="$(/usr/bin/plutil -extract identity raw -o - "${SIGNING_CONFIG}")"
KEYCHAIN="$(/usr/bin/plutil -extract keychain raw -o - "${SIGNING_CONFIG}")"
if [[ ! "${IDENTITY}" =~ ^[0-9A-Fa-f]{40}$ ]]; then
  echo "${SIGNING_CONFIG} has no SHA-1 identity; refusing to sign ad hoc" >&2
  exit 1
fi
if ! security find-identity -p codesigning "${KEYCHAIN}" 2>/dev/null | grep -qi "${IDENTITY}"; then
  echo "the pinned identity is not in ${KEYCHAIN}; refusing to sign ad hoc" >&2
  exit 1
fi

cd "${REPO_DIR}"
taskpolicy -c utility cargo build -j 3 --offline

mkdir -p "${MACOS_DIR}" "${RESOURCES_DIR}" "${HOME}/bin"
# Annotator ships without Sparkle; drop a framework an older build left behind.
rm -rf "${FRAMEWORKS_DIR}"

rm -f "${MACOS_DIR}/scrawl" "${MACOS_DIR}/scrawl-helper"
cp -f "${REPO_DIR}/target/debug/scrawl" "${MACOS_DIR}/scrawl"
cp -f "${REPO_DIR}/target/debug/scrawl-helper" "${MACOS_DIR}/scrawl-helper"
chmod +x "${MACOS_DIR}/scrawl" "${MACOS_DIR}/scrawl-helper"

if [[ -f "${REPO_DIR}/assets/Scrawl.icns" ]]; then
  cp -f "${REPO_DIR}/assets/Scrawl.icns" "${RESOURCES_DIR}/Scrawl.icns"
fi

cat > "${CONTENTS_DIR}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>scrawl</string>
  <key>CFBundleIconFile</key>
  <string>Scrawl</string>
  <key>CFBundleIdentifier</key>
  <string>${BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>${APP_NAME}</string>
  <key>CFBundleDisplayName</key>
  <string>${APP_NAME}</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>15.0</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSScreenCaptureUsageDescription</key>
  <string>Scrawl captures your screen to build annotated agent bundles.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Scrawl records your voice to align transcripts with keyframes.</string>
  <key>NSCameraUsageDescription</key>
  <string>Scrawl mirrors and records a cabled iPhone's screen.</string>
  <key>NSSpeechRecognitionUsageDescription</key>
  <string>Scrawl transcribes your narration on this Mac.</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>Scrawl reads the address of the browser tab you captured, to send a capture of a dev site to its project's session.</string>
</dict>
</plist>
PLIST

# No hardened runtime: this is a local build, and the runtime would need the release entitlements.
codesign --force --sign "${IDENTITY}" --keychain "${KEYCHAIN}" --identifier "${HELPER_ID}" \
  --requirements "=designated => identifier \"${HELPER_ID}\" and certificate leaf = H\"${IDENTITY}\"" \
  "${MACOS_DIR}/scrawl-helper"
codesign --force --sign "${IDENTITY}" --keychain "${KEYCHAIN}" --identifier "${BUNDLE_ID}" \
  --requirements "=designated => identifier \"${BUNDLE_ID}\" and certificate leaf = H\"${IDENTITY}\"" \
  "${APP_DIR}"
codesign --verify --strict "${APP_DIR}"

if [[ "${SCRAWL_NO_LAUNCH:-0}" == "1" ]]; then
  exit 0
fi

for link in "${CLI_LINKS[@]}"; do
  ln -sf "${MACOS_DIR}/scrawl" "${link}"
done

scrawl_running() {
  local binary status
  for binary in "${APP_BINARIES[@]}"; do
    status=0
    pgrep -f "${binary}" >/dev/null || status=$?
    case "${status}" in
      0) return 0 ;;
      1) ;;
      *) echo "pgrep failed (exit ${status}); can't tell whether the old app stopped" >&2; exit 1 ;;
    esac
  done
  return 1
}

for binary in "${APP_BINARIES[@]}"; do
  pkill -f "${binary}" 2>/dev/null || true
done
pkill -f "target/debug/scrawl" 2>/dev/null || true
for _ in {1..40}; do
  scrawl_running || break
  sleep 0.05
done
if scrawl_running; then
  for binary in "${APP_BINARIES[@]}"; do
    pgrep -fl "${binary}" >&2 || true
  done
  echo "the old Scrawl or Annotator is still running; quit it and run this again" >&2
  exit 1
fi
open -a "${APP_DIR}" || (sleep 0.3 && open -n -a "${APP_DIR}")
sleep 0.3
pgrep -fl "${APP_NAME}.app/Contents/MacOS/scrawl"
