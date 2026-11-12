# summon

summon 把本地 AI CLI 接到飞书机器人，支持会话续接、流式回复、定时任务和多 agent 管理。
CLI、daemon 和 Go sidecar 支持 macOS、Linux；Menu Bar 应用支持 macOS。

各版本的新增功能、改进与修复见 [Release Notes](RELEASE_NOTES.md)，从 0.6.20 起记录。

## 运行环境

- 至少一个已配置的 AI CLI：Claude Code、Codex、Cursor 或 Trae。
- 使用 Claude Code 的交互会话时需要 tmux 3.6 或更新版本。
- 飞书开放平台应用及相应权限。
- `lark-cli` 用于需要飞书工具的功能，可从其官方渠道安装。

## 文档评论

bot 可将自己拥有的文档的新评论通知 owner，并通过 Claude Code 或 Codex 起草回复。
owner 审阅并明确批准后，由同一 bot 回复原评论。Cursor 不支持文档评论自动草稿。

## 从源码构建

准备 Rust stable、Go（版本要求见 `sidecar/feishu-connector/go.mod`）。
macOS 上构建 Menu Bar 还需要 Swift 6 工具链。

```sh
cargo build --release --locked
(cd sidecar/feishu-connector && go build -trimpath -o ../../target/release/summon-sidecar .)
# macOS 可选
swift build --package-path menubar -c release
```

需要安装到本机并启动服务时，运行安装脚本。它会更新 `~/.summon/bin/` 并重启 daemon；
升级时重新运行同一条命令。

```sh
sh scripts/deploy.sh --install
# macOS 可选：同时安装 Menu Bar
sh scripts/deploy.sh --install --menubar
```

## 配置与使用

```sh
summon onboard
summon doctor
summon guide
```

`onboard` 引导创建机器人配置并连接本地 CLI。在飞书私聊中直接发消息，在群聊中提及机器人。
具体命令、配置项和故障排查使用随二进制分发的手册：

```sh
summon guide commands
summon guide config
summon guide troubleshooting
```

运行数据保存在 `~/.summon/`，包括机器人凭证、身份资料、会话记录和定时任务。
这些文件由使用者在本地管理。请把源码工作目录与运行数据目录分别维护。

## 开发与验证

模块边界见 [架构说明](docs/architecture.md)，开发规则见 [CLAUDE.md](CLAUDE.md)。

```sh
bash scripts/verify.sh
python3 scripts/check-public-content.py
```

`verify.sh` 执行本地机器检查。需要真实服务登录态的测试由开发者单独选择测试环境运行。
提交内容使用合成数据；凭证通过本机配置注入。`check-public-content.py` 检查凭据、
像真实的平台标识、本机路径与邮箱等不该出现在源码里的内容。

## 许可证

使用 MIT 许可证，见 [LICENSE](LICENSE)。第三方资源的许可证保存在对应资源目录。
