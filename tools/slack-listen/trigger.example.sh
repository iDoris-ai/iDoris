#!/bin/sh
# Example trigger. The listener passes one event JSON object on stdin.
# This script does not call a model and does not post to Slack.
umask 077
dir="${HOME}/.local/state/slack-listen"
mkdir -p "$dir"
cat >> "$dir/trigger.log"
printf '\n' >> "$dir/trigger.log"
