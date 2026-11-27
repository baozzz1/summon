# 配置 summon

简体中文 | [English](configuration.en.md)

summon 的行为由 `~/.summon/` 下的 YAML 配置决定。字段说明、默认值和生效时机都写在 Summon Guide 里。
Summon Guide 是随二进制分发的使用手册，内容与本机装的版本一致。

**推荐让 agent 来配置 summon。** 在飞书里告诉 agent 你想要什么效果，由它查手册、改配置、验证结果。
手动修改也可以，但要自己核对字段名、覆盖层级和生效时机。

## 让 agent 配置（推荐）

summon 给每个接入的 agent 的开场约定里都写明了：用户问到配置、开关或生效时机时，先读 `summon guide config`；
遇到故障时，先读 `summon guide troubleshooting`。所以只需要描述你要的结果，不用先记字段名。例如：

- 「这个群里别显示工具调用详情。」
- 「会话空闲两小时就回收。」
- 「每轮开跑前确认本机 18765 端口的代理在运行，没起来就交给 launchd 拉起。」
- 「`summon doctor` 报了一条警告，帮我看看怎么修。」

agent 通常会按这个顺序处理：

1. 读 `summon guide config`，确认对应字段、可选值和生效时机。
2. 读当前配置，找出这个值来自内置默认、全局配置、单 agent 配置还是本会话覆盖。
3. 说明打算怎么改，得到你的同意后再改 `config.yaml` 或 `agent.yaml`。
4. 按生效时机验证：等下一轮、重连会话或重启 daemon，再跑 `summon doctor` 体检。

agent 做不了的事：在飞书开放平台创建和发布应用，这一步要你手工完成，见 `summon guide manage`。
另外，消息协议这类内置约定不能通过配置覆盖。

## Summon Guide

在终端运行 `summon guide` 看索引，运行 `summon guide <章节>` 看具体章节。下面的链接指向仓库 `main` 分支上的手册源文件，
可能比你本机装的版本新；以本机 `summon guide` 的输出为准。

| 章节 | 内容 |
| --- | --- |
| [`summon guide`](../locales/assets/zh-CN/guide/index.md) | 索引：心智模型、各章节入口、能力边界 |
| [`summon guide config`](../locales/assets/zh-CN/guide/config.md) | 配置字段、覆盖层级与生效时机，以及什么情况下建议配什么 |
| [`summon guide commands`](../locales/assets/zh-CN/guide/commands.md) | 飞书里的斜杠命令、按钮与打断 |
| [`summon guide manage`](../locales/assets/zh-CN/guide/manage.md) | 终端管理：新建 agent（onboard / agents add / bind）、daemon 运维 |
| [`summon guide troubleshooting`](../locales/assets/zh-CN/guide/troubleshooting.md) | 常见症状、排查方法与修复 |
| [`summon guide protocol`](../locales/assets/zh-CN/guide/protocol.md) | agent 要遵守的操作规程：定时器、会话前文、附件、跨群委派 |
| [`summon guide handoff`](../locales/assets/zh-CN/guide/handoff.md) | `/handoff` 交接文件的模板与标签 |
| [`summon guide mentionless`](../locales/assets/zh-CN/guide/mentionless.md) | 一人一 Bot 群开启免 @ 的检查项与权限申请 |
| [`summon guide quotes`](../locales/assets/zh-CN/guide/quotes.md) | 按 ID 读取被引用消息的原文 |
| [`summon guide reactions`](../locales/assets/zh-CN/guide/reactions.md) | 给消息贴语义表情 |

## 配置文件

| 文件 | 作用范围 |
| --- | --- |
| `~/.summon/config.yaml` | 全局，影响所有 agent。只写要覆盖的字段，其余沿用内置默认值 |
| `~/.summon/agents/<alias>/agent.yaml` | 单个 agent：身份、飞书凭据环境变量名、运行时（CLI、工作目录、环境变量、前置服务依赖）和消息配置 |
| `~/.summon/agents/<alias>/.env` | 这个 agent 的凭据，例如飞书应用的 App ID 与 App Secret |
| `~/.summon/.env` | 全局环境变量，供 `config.yaml` 里的 `${VAR}` 引用 |

消息相关的开关按「内置默认 → 全局 → agent → 本会话」逐层覆盖。只有部分开关支持在飞书里用 `/set` 做本会话覆盖。
各字段的生效时机不同：有的下一轮生效，有的要在会话重连或 daemon 重启后生效。具体见 `summon guide config` 的「覆盖层级与生效时机」。

除了直接编辑 YAML，本机 Web 控制台和 macOS Menu Bar 的 Settings 区也能修改其中一部分字段，写入的是同一份配置文件。

## 手动修改

1. 运行 `summon guide config`，确认字段名、可选值和生效时机。
2. 编辑 `config.yaml` 或对应的 `agent.yaml`，先保留原值，方便回退。
3. 按生效时机等待下一轮、重连会话或重启 daemon，然后实际验证一次。
4. 运行 `summon doctor` 检查配置。

## 示例：声明前置依赖服务

有些 agent 依赖本机常驻服务，例如把 CLI 接到其它模型的本地代理。这类服务在全局 `config.yaml` 的 `services` 里声明，
再在依赖它的 agent 的 `agent.yaml` 里用 `runtime.requires` 引用。每轮开跑前，summon 会先探活：服务健康就直接开跑；
不健康时执行兜底命令，仍不就绪则本轮直接报错，不会用一个跑不通的环境去开跑。

服务由 launchd 常驻托管时，可以这样配置：

```yaml
# ~/.summon/config.yaml
services:
  local-proxy:
    healthcheck:
      http: "http://127.0.0.1:18765/healthz"
    # 探活失败时交给 launchd 重启；oneshot 表示这是一次性交接命令，进程仍归 launchd 管
    command: ["/bin/launchctl", "kickstart", "-k", "gui/501/com.example.local-proxy"]
    oneshot: true
```

```yaml
# ~/.summon/agents/<alias>/agent.yaml
runtime:
  requires:
    - local-proxy
```

把 `gui/501` 里的 `501` 换成你的用户 ID（`id -u`）。配好后，用 `summon services status` 查看状态，用 `summon services check` 做一次探活。
完整字段，包括 `tcp` 和 `command` 两种探活方式、`agent_env` 环境变量注入和 daemon 重启时的进程保留，见 `summon guide config` 的「整机服务与连接」。
