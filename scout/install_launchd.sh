#!/usr/bin/env bash
# Install (or reinstall) the scout as a macOS launchd agent that runs every
# six hours and on login. Idempotent. `./install_launchd.sh --uninstall` removes it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LABEL="com.webcortex.scout"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
PYTHON="${SCOUT_PYTHON:-$(command -v python3)}"
INTERVAL="${SCOUT_INTERVAL_SECS:-21600}"
MODEL="${SCOUT_MODEL:-qwen3.5:9b}"

if [[ "${1:-}" == "--uninstall" ]]; then
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  echo "removed $LABEL"
  exit 0
fi

mkdir -p "$HERE/logs" "$HOME/Library/LaunchAgents"
cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$PYTHON</string>
    <string>$HERE/scout.py</string>
  </array>
  <key>WorkingDirectory</key><string>$HERE</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>SCOUT_MODEL</key><string>$MODEL</string>
    <key>PATH</key><string>/usr/local/bin:/usr/bin:/bin:$HOME/.local/bin</string>
  </dict>
  <key>StartInterval</key><integer>$INTERVAL</integer>
  <key>RunAtLoad</key><true/>
  <key>Nice</key><integer>10</integer>
  <key>StandardOutPath</key><string>$HERE/logs/scout.log</string>
  <key>StandardErrorPath</key><string>$HERE/logs/scout.err</string>
</dict>
</plist>
PLIST

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "installed $LABEL: every $INTERVAL s with model $MODEL"
echo "  logs:        $HERE/logs/"
echo "  suggestions: $HERE/suggestions.md"
echo "  run now:     launchctl kickstart -k gui/$(id -u)/$LABEL"
