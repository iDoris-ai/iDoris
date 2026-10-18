#!/bin/bash
# Install the per-user LaunchAgent. Does not print secrets.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
ENV_FILE="${SLACK_ENV_FILE:-$HOME/.config/grok-slack/env}"
LABEL="com.aastar.slack-listen"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/slack-listen.log"

if [[ -x "$HOME/.pyenv/versions/3.11.9/bin/python3" ]]; then
  PY="${SLACK_LISTEN_PYTHON:-$HOME/.pyenv/versions/3.11.9/bin/python3}"
else
  PY="${SLACK_LISTEN_PYTHON:-$(command -v python3)}"
fi

if [[ ! -f "$ENV_FILE" ]]; then
  echo "没有密钥文件：$ENV_FILE"
  exit 1
fi

if ! SLACK_ENV_FILE="$ENV_FILE" "$PY" -c '
import os
from pathlib import Path
path = Path(os.environ["SLACK_ENV_FILE"])
ok = False
for raw in path.read_text(encoding="utf-8").splitlines():
    line = raw.strip()
    if not line.startswith("SLACK_APP_TOKEN="):
        continue
    value = line.split("=", 1)[1].strip().strip("'\''").strip("\"")
    ok = value.startswith("xapp-")
    break
raise SystemExit(0 if ok else 1)
'; then
  cat <<'EOF'
还不能安装守护进程。密钥文件里没有可用的 SLACK_APP_TOKEN。

在 https://api.slack.com/apps 打开你的 Slack 应用，然后：
1. 打开 Socket Mode。
2. Basic Information → App-Level Tokens → Generate，scope 勾 connections:write。
3. 把 xapp- 开头的令牌写入密钥文件的 SLACK_APP_TOKEN。不要把令牌贴进对话。
4. Event Subscriptions 打开，Subscribe to bot events 增加 message.channels、message.groups、app_mention。
5. Slack 如果要求重新安装到工作区，按它的提示做。
6. 在 #pr 输入 /invite @你的bot。

写好之后再运行一次 install.sh。
EOF
  exit 1
fi

mkdir -p "$HOME/Library/LaunchAgents" "$(dirname "$LOG")"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$PY</string>
    <string>$ROOT/listen.py</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>$LOG</string>
  <key>StandardErrorPath</key>
  <string>$LOG</string>
  <key>WorkingDirectory</key>
  <string>$ROOT</string>
</dict>
</plist>
EOF

UID_NUM="$(id -u)"
launchctl bootout "gui/$UID_NUM/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$UID_NUM" "$PLIST"
launchctl enable "gui/$UID_NUM/$LABEL"
launchctl kickstart -k "gui/$UID_NUM/$LABEL"
echo "已安装 $LABEL"
echo "日志：$LOG"