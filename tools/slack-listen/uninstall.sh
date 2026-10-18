#!/bin/bash
# Remove the per-user LaunchAgent. Leaves the local spool in place.
set -euo pipefail

LABEL="com.aastar.slack-listen"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
UID_NUM="$(id -u)"
launchctl bootout "gui/$UID_NUM/$LABEL" 2>/dev/null || true
rm -f "$PLIST"
echo "已卸下 ${LABEL}。本地事件记录还在 ~/.local/state/slack-listen/"
