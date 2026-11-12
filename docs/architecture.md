# 架构说明

summon 在本机维护 AI CLI 会话，通过飞书机器人接收入站任务并投递结构化回复。

## 组件与数据

Rust CLI 提供配置、管理和诊断入口。daemon 负责长连接事件分发、会话调度、
定时触发与消息投递。Go sidecar 负责飞书长连接；macOS Menu Bar 展示 daemon 的会话快照。
配置及运行数据位于使用者的 `~/.summon/`。

`summon-core` 提供配置解析、CLI 输出解析、会话标识、投递与状态相关的领域类型。
`summon` 将这些类型与文件系统、子进程、IPC、飞书 API 和用户命令连接起来。

## 会话与投递

Claude Code 的交互会话运行于 tmux，daemon 读取结构化转录。
Codex 和 Cursor 使用各自支持的 RPC 通道，进程与会话按后端契约管理。
回复、过程内容和子 agent 事件分别跟踪投递状态。

入站任务持久化后进入所属会话。恢复、接管和补发必须证明身份及处理归属，
避免重复执行或向错误会话发送历史内容。
凭据按 agent 隔离；影响会话生命周期的操作走统一授权与 teardown 路径。

## 安装

`scripts/deploy.sh --install` 从源码构建并把 `summon`、`summon-sidecar`、`summon-menubar`
发布到稳定落点 `~/.summon/bin/`，开机自启单元只登记这个落点。落点旁的构建戳记录
落点文件的版本与提交，发布前据此判断新旧，绝不拿旧的覆盖新的。

## 配置与手册

`locales/assets/<language>/guide/` 是使用手册的源码，由 `summon guide` 按章节提供。
全局、agent 和会话层配置按各字段定义的生效时机读取。
增加配置字段时同时检查默认值、序列化兼容、验证、手册和测试。

## 设计记录

源码注释引用的 `docs/spec/`、`docs/superpowers/` 等设计记录不在本仓库中；
相关的约束与原因写在引用它们的注释里。
