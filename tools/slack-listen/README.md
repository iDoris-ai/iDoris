# Slack 用法

不用打开 Slack 桌面客户端。发消息、@ 同事、读历史，由 Grok 在对话里调用本机 MCP。后台监听单独负责收消息，写到本机文件。两套都用 `~/.config/grok-slack/env` 里的令牌，令牌不要写进仓库，也不要贴进对话。

## 现在能做什么

发和读走 MCP，程序是 `~/.config/grok-slack/server.py`，在 `~/.grok/config.toml` 的 `mcp_servers.slack` 里注册。新开一个 Grok 会话后才能用。

- 以你的名义发到指定频道。默认用用户令牌，Slack 里显示为你本人。你要在对话里点名频道和正文，Grok 才发。
- @ 同事。先查出对方的用户 id，正文写成 `<@U…>`。只写 `@名字` 不会变成提及。
- 读某个频道的最近消息，或按人搜索。
- 用 bot 身份发。需要明确说用 bot 发，显示为这个应用的 bot。你这边的 bot 是 `@grokslack`。

收消息走本目录的监听进程。它是用户级 LaunchAgent `com.aastar.slack-listen`，登录后运行，不调用模型，不发 Slack。

- `#pr` 里其他人的新消息会追加到 `~/.local/state/slack-listen/events.jsonl`。
- 别人 @ 你的 bot 时，这条提及也会记进同一个文件。Slack 里要 @ 的是 `@grokslack`，不是一个叫 grokbot 的账号。
- 你自己发出的消息、bot 自己的消息，监听会丢掉，避免自己触发自己。
- Grok 不会因为文件里多了一行就自己开一轮对话或自动回复。要让它处理，就在对话里让它读这个文件。`trigger` 现在是空的。不要把 `trigger` 指到一条会启动 Grok、并且握有 Slack 读或写工具、又没有人工确认的命令。频道里的原文会原样交给这条命令，别人可以借此让它以你的名义发言。只有这种命令才会消耗 Grok token；监听本身不消耗。

Grok Bot 应用里的 Slack connector 是第三条路。它只留在 Grok Bot 里，不给这个终端登录，也不会变成频道里那个可 @ 的 bot。

## 同事要复制全部功能

他在自己的电脑上做，用自己的 Slack 账号。不要复制你的 `env`，不要共用用户令牌或 bot 令牌。

1. 加入同一个 Slack 工作区，进入 `#pr`。网页登录即可，不必装桌面客户端。
2. 在 https://api.slack.com/apps 新建自己的 Slack App。Redirect URL 填 `http://127.0.0.1:8787/slack/callback`。User scopes 和 Bot scopes 与已经配好的那台机器上 `~/.config/grok-slack/auth.py` 保持一致。
3. `server.py` 和 `auth.py` 不在这个仓库里，密钥也不在。从已经配好的那台机器复制这两个程序到他本机的 `~/.config/grok-slack/`，不要复制 `env`。`env` 里先写他自己的 `SLACK_APP_ID`、`SLACK_CLIENT_ID`、`SLACK_CLIENT_SECRET`、`SLACK_TEAM_ID`。运行 `python3 auth.py`，用他自己的账号点 Allow。这一步写入他的 `SLACK_USER_TOKEN` 和 `SLACK_BOT_TOKEN`。
4. 打开 Socket Mode。生成 App-Level Token，scope 勾 `connections:write`。值必须写在 `SLACK_APP_TOKEN` 这一键上，`xapp-` 开头。不要写到 `SLACK_USER_TOKEN` 上。
5. Event Subscriptions 打开，bot events 增加 `message.channels`、`message.groups`、`app_mention`。Slack 要求重新安装到工作区时照做。
6. 在 `#pr` 输入 `/invite @他的bot`。bot 不在频道里时，监听连得上，但收不到这个频道的消息。
7. 在他的 `~/.grok/config.toml` 注册 `mcp_servers.slack`，`command` 指向他的 Python，`args` 指向他的 `server.py`。新开一个 Grok 会话后，他可以让 Grok 以他的名义发消息、@ 人、读频道。
8. 复制本目录 `tools/slack-listen/`，在他的电脑上运行 `install.sh`。他的监听只触发他本机的记录和命令。你的监听不会替他运行。

他发出的用户消息会出现在你的 `events.jsonl`。你发出的用户消息会出现在他的 `events.jsonl`。

## 密钥

`~/.config/grok-slack/env` 需要这三行，文件权限 `600`：

- `SLACK_APP_TOKEN`：`xapp-` 开头，只给监听开 Socket Mode。
- `SLACK_USER_TOKEN`：`xoxp-` 开头。发消息时代表你，监听用它识别“自己”并解析频道名。
- `SLACK_BOT_TOKEN`：`xoxb-` 开头。以 bot 发言，并让监听识别 bot 自己的用户 id。

## 安装和配置

```bash
tools/slack-listen/install.sh
tools/slack-listen/uninstall.sh
```

日志在 `~/Library/Logs/slack-listen.log`。状态在 `~/.local/state/slack-listen/status.json`。

这是用户级 LaunchAgent。登录后启动，进程异常退出后会拉起来。退出登录后它会停。机器睡眠时套接字会断，醒来后自己重连。它不是 root 服务，也不属于 iDoris 的路由程序。

可选配置复制 `listen.example.json` 为 `~/.config/grok-slack/listen.json`。不复制时默认只听 `#pr`。`users` 填了就只留这些人。`mentions_only` 为 true 时只留提及。带 subtype 的消息不记，包括编辑、删除、bot 消息、`file_share` 和 `thread_broadcast`。

`trigger.example.sh` 只把 JSON 再写一份到 `trigger.log`。要启用它，把配置里的 `trigger` 写成这个脚本的绝对路径，并 `chmod +x`。选 Socket Mode 是因为本机不需要公网地址。
