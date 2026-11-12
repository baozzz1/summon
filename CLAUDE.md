# summon 开发规则

默认用简体中文沟通。代码与文档描述当前行为，注释解释不显然的原因。

## 模块

- `crates/summon-core`：配置、解析、消息和会话领域逻辑。
- `crates/summon`：CLI、daemon、会话执行、飞书前端和本机安装。
- `sidecar/feishu-connector`：飞书长连接与 IPC。
- `menubar`：macOS 原生会话面板。
- `locales/assets/<language>/guide`：随二进制分发的使用手册。

## 修改与验收

使用独立分支或 worktree 开发，各 worktree 使用自己的构建目录。
检查结果绑定提交或树；内容发生变化时，按影响补查。

局部低风险改动运行格式检查、受影响测试和 crate lint。
涉及持久化、并发、进程生命周期、鉴权、主动外发、公共消息路由或跨模块契约时，
运行相关全量机器检查并完成独立 review。

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets --locked
bash scripts/verify.sh
python3 scripts/check-public-content.py
```

测试删减必须说明保留覆盖的对应关系。安全边界、兼容性和错误路径的独立覆盖必须保留。
真实 CLI、外部服务和设备测试使用明确授权的独立测试环境。

## 运行边界

- daemon 重启保留 tmux agent 会话，使用 `summon daemon stop` 和 `summon daemon start`。
- 会话退出、归档、重置和打断走现有受控入口，保留各自的鉴权、持久化和 teardown 判据。
- 无头进程打断只处理其子进程组。软打断优先，保留收敛宽限。
- 主动外发路径默认 fail-closed，发送前证明目标身份、准入条件和投递归属。
- agent 子进程只取得自己的凭据。凭据值与片段均不得进入日志、测试数据或审查材料。
- 测试输入使用合成姓名、消息、身份标识和路径，保持结构及断言覆盖。
- 二进制图片新增或变更时，检查全部帧与元数据，并在 `scripts/check-public-content.py`
  里更新经审查的摘要。
- 运行数据和个人上下文在源码仓库之外保存。

## 提交

提交前检查暂存区内容、隐私扫描结果、许可证及第三方署名。
提交消息只描述最终行为。
