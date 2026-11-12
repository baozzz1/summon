# 内置语言资源

全局配置 `language` 接受 `auto`、`zh-CN`、`en`。`auto` 通过配置的 lark-cli 执行
`config show` 并读取 `.lang`；读取失败或没有值时取系统第一首选语言。中文归为简体中文，
其它语言归为英语。控制台配置表修改后立即生效，无需重启 daemon。

- `shared/`：语言选项和跨端基础标签。
- `rust/`：CLI、配置元数据、飞书卡片、诊断和运行时消息。
- `dashboard/`：前端文字及日期标签。
- `assets/<language>/`：随二进制分发的手册、提示词和配置示例。
- 菜单栏资源在 `menubar/Sources/SummonMenuBar/LocalizationResources.swift`。
- Sidecar 资源在 `sidecar/feishu-connector/localization_resources.go`。

两种语言使用同一组 key。Rust 的 `summon_i18n::text!` 和 `format!` 在编译时读取资源，
分别提供静态文字和保留编译期检查的格式串；静态元数据存 key，在显示边界调用
`i18n::label`。`include_text!` 嵌入两种语言的文本资产。前端 `rich` 保留 Lit 值与用户文本，
翻译资源不会作为 HTML 解析。

机器协议、命令拼写、解析别名、历史记录标记和用户内容保持原值；`source!` 仅用于固定的
源码标记。路由和授权使用结构化类型，不比较译文。历史诊断日志同时识别两种语言，
白名单模板的占位符顺序必须一致，以保持脱敏位置。

单次同步渲染可用 `with_language` 冻结语言；daemon 采用代际围栏更新全局语言。
Sidecar 只读 `run/display-language` 派生缓存，配置权威仍是 `config.yaml`。
菜单栏从既有 fleet 快照读取语言，开着的菜单在关闭后重建。

验证入口：Rust `i18n::tests`、`runtime::localization::tests` 和日志导出测试；
前端 `src/i18n/i18n.test.ts` 与 Store 测试；Swift `LocalizationTests`；Go localization 测试。
截图脚本支持 `SUMMON_SHOT_LANGUAGE=en`，在示例环境检查英文及滚动布局。
