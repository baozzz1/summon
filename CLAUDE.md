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

## 提交与合入

提交前检查暂存区内容、隐私扫描结果、许可证及第三方署名。
提交消息只描述最终行为。

`main` 只经 PR 合入：在分支上开发，推到 GitHub 开 PR。`checks` 工作流的 Linux 与 macOS
两个 job 都通过、review 批准后，由作者（包括 agent）自行合入。不直接推送或强推 `main`。

## 发版

发版全部在 GitHub Actions 的 `release` 工作流里完成。版本号只用纯数字的 `x.y.z`：
已安装的启动器只认这个形态。

1. 开一个发版 PR：改 `crates/summon/Cargo.toml` 的版本与 `Cargo.lock` 里 summon 的版本，
   把 `RELEASE_NOTES.md`「未发布」里的条目移到新的版本节 `## x.y.z · 日期`。
2. 合入后，在 `main` 的合入提交上打 tag 并推送：`git tag vX.Y.Z <提交>`、`git push origin vX.Y.Z`。
   tag 不在 `main` 上，或与 `Cargo.toml` 的版本不符，工作流直接失败。
3. 工作流建草稿 Release、构建两个 Linux 包与未签名的 macOS 二进制；签名 job 绑定 environment
   `release`，owner 在 GitHub 上批准后才导入签名证书、签名打包，之后另一个 job 解包做冒烟检查。三个包都到齐后核对字节数、SHA-256、
   提交与包成员，生成并上传 `manifest.json`，再转为发布；latest 始终指向已发布的最高版本。
4. 任何一步失败，Release 都停在草稿，对外不可见；修好后在 Actions 里重跑失败的 job。
   已发布的 Release 不改写，要换内容就发下一个版本。

演练：对 `main` 上的提交推送 tag `test-<提交号前缀>`（7 到 40 位），走完同样的流程（签名 job
同样要批准），停在 prerelease 草稿，永不转为发布。看完删掉草稿和 tag。

签名的部署批准只由 owner 本人在 GitHub 网页上点，agent 不调用部署审批接口（`pending_deployments`
等）。tag 触发的运行用的是被打 tag 那个提交里的工作流，environment 只看 tag 名，所以批准之前核对：
这次运行的提交在 `main` 上，且它的 `.github/workflows/`、`scripts/github-release/` 与 `main` 一致。
签名 job 只运行取自 `main` 的签名脚本，不构建、不执行被签名的二进制。

签名证书由 Apple「Developer ID Certification Authority」（G1）中间证书签发，`macos-keychain.sh`
只钉了这一张。换成 G2 签发的证书后，导入会报「0 个」有效身份，届时把钉住的地址和摘要换成 G2 的。

发布脚本在 `scripts/github-release/`，改动后跑 `scripts/verify.sh` 第一步里的脚本测试。
