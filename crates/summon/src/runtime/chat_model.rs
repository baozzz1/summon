//! 本群模型与思考档位:覆盖文件(`threads/<tag>/model`、`threads/<tag>/effort`)与「本群生效值」。
//!
//! 取值链是 本群覆盖 > bot 默认(`agent.yaml`)> CLI 自定。启动参数、每轮请求、面板、角标都经
//! [`effective`] 取值,不各自读 `agent.yaml`。Cursor 参数化 ACP 的本群覆盖是另一种形态(稀疏
//! bundle),见 `capture::cursor_config`。设计见 `docs/spec/2026-10-20-per-chat-model-design.md`。

use std::path::PathBuf;
use summon_core::registry::Agent;

use crate::observe::log::log;

/// 值来自哪一层。CLI 自定也算 `Default` —— 那是「本群没有覆盖」的一种,面板另行标注。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer {
    Chat,
    Default,
}

/// 本群生效的 model 与 effort,连同各自来自哪一层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Effective {
    pub model: Option<String>,
    pub model_layer: Layer,
    pub effort: Option<String>,
    pub effort_layer: Layer,
}

impl Effective {
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }
}

/// 本群生效值。`tag` 为 None(不属于任何 chat 的会话)时就是 bot 默认。
pub(crate) fn effective(agent: &Agent, tag: Option<&str>) -> Effective {
    let chat_model = tag.and_then(|tag| chat_model(agent, tag));
    let chat_effort = tag.and_then(|tag| chat_effort(agent, tag));
    Effective {
        model_layer: layer_of(chat_model.is_some()),
        model: chat_model.or_else(|| agent.model().map(str::to_string)),
        effort_layer: layer_of(chat_effort.is_some()),
        effort: chat_effort.or_else(|| agent.effort().map(str::to_string)),
    }
}

fn layer_of(overridden: bool) -> Layer {
    if overridden {
        Layer::Chat
    } else {
        Layer::Default
    }
}

/// 本群覆盖所在目录。tag 由 `registry::chat_tag` 生成(`g`/`p` + 十六进制);不像这个形态的一律
/// 不当路径用,免得 `..` 或带分隔符的值把读写引到 threads 之外。
pub(crate) fn chat_dir(agent: &Agent, tag: &str) -> Option<PathBuf> {
    let safe = !tag.is_empty() && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    safe.then(|| agent.state_dir().join("threads").join(tag))
}

/// 本群显式设置的模型。读不到、内容不像模型 id 都当没设(退回默认无害),后者留日志 ——
/// 手改出来的坏值不该被当成模型名拼进启动参数。
pub(crate) fn chat_model(agent: &Agent, tag: &str) -> Option<String> {
    let value = read_value(agent, tag, "model")?;
    match summon_core::model_effort::check_model_id(&value) {
        Ok(()) => Some(value),
        Err(error) => {
            log(&summon_i18n::format!("rff838cbcd4", agent.name));
            None
        }
    }
}

/// 本群显式设置的 effort。读不到当没设:effort 只是思考档位,退回默认无害。
pub(crate) fn chat_effort(agent: &Agent, tag: &str) -> Option<String> {
    read_value(agent, tag, "effort")
}

fn read_value(agent: &Agent, tag: &str, name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(chat_dir(agent, tag)?.join(name)).ok()?;
    let value = raw.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// 写本群模型覆盖;`None` 清除(回到默认)。调用方已做过形态校验与候选匹配。
pub(crate) fn set_chat_model(agent: &Agent, tag: &str, model: Option<&str>) -> anyhow::Result<()> {
    write_value(agent, tag, "model", model)
}

/// 写本群 effort 覆盖;`None` 清除(回到默认)。调用方已做过合法性校验。
pub(crate) fn set_chat_effort(
    agent: &Agent,
    tag: &str,
    effort: Option<&str>,
) -> anyhow::Result<()> {
    write_value(agent, tag, "effort", effort)
}

fn write_value(agent: &Agent, tag: &str, name: &str, value: Option<&str>) -> anyhow::Result<()> {
    let dir =
        chat_dir(agent, tag).ok_or_else(|| anyhow::anyhow!(summon_i18n::format!("rb2e4b322f6")))?;
    let path = dir.join(name);
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => {
            std::fs::create_dir_all(&dir)?;
            crate::runtime::fsio::atomic_write(&path, format!("{value}\n").as_bytes())
        }
        None => match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        },
    }
}

/// 设了本群模型覆盖的群有几个(改默认的回执用)。`except` 是发起改动的那个群,不计入。
pub(crate) fn chats_with_model_override(agent: &Agent, except: Option<&str>) -> usize {
    count_chats(agent, except, |tag| chat_model(agent, tag).is_some())
}

/// 同上,effort。
pub(crate) fn chats_with_effort_override(agent: &Agent, except: Option<&str>) -> usize {
    count_chats(agent, except, |tag| chat_effort(agent, tag).is_some())
}

fn count_chats(agent: &Agent, except: Option<&str>, has: impl Fn(&str) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(agent.state_dir().join("threads")) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|tag| Some(tag.as_str()) != except)
        .filter(|tag| has(tag))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_in(dir: &std::path::Path, yaml: &str) -> Agent {
        let agent_dir = dir.join("agents").join("examplebot");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.yaml"), yaml).unwrap();
        let mut agent: Agent = serde_yaml::from_str(yaml).unwrap();
        agent.dir = agent_dir;
        // 先建 state 目录:state_dir() 按盘上现状在新旧位置之间回退,不钉住会在测试中途改口。
        std::fs::create_dir_all(agent.state_dir()).unwrap();
        agent
    }

    #[test]
    fn a_chat_override_wins_over_the_bot_default_only_in_its_own_chat() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(
            tmp.path(),
            "name: examplebot\ncli: claude\nmodel: opus\neffort: high\n",
        );
        let default = effective(&agent, Some("g-a"));
        assert_eq!(default.model(), Some("opus"));
        assert_eq!(default.model_layer, Layer::Default);

        set_chat_model(&agent, "g-a", Some("sonnet")).unwrap();
        set_chat_effort(&agent, "g-a", Some("low")).unwrap();
        let a = effective(&agent, Some("g-a"));
        assert_eq!((a.model(), a.model_layer), (Some("sonnet"), Layer::Chat));
        assert_eq!((a.effort(), a.effort_layer), (Some("low"), Layer::Chat));
        let b = effective(&agent, Some("g-b"));
        assert_eq!((b.model(), b.model_layer), (Some("opus"), Layer::Default));
        assert_eq!((b.effort(), b.effort_layer), (Some("high"), Layer::Default));
        assert_eq!(effective(&agent, None).model(), Some("opus"));
        assert_eq!(chats_with_model_override(&agent, None), 1);
        assert_eq!(chats_with_model_override(&agent, Some("g-a")), 0);

        set_chat_model(&agent, "g-a", None).unwrap();
        set_chat_model(&agent, "g-a", None).unwrap();
        assert_eq!(effective(&agent, Some("g-a")).model(), Some("opus"));
    }

    /// 消费点取本群生效值的接线。行为测试覆盖了 claude 启动参数与 Cursor 收敛;codex 每轮请求与
    /// bridge 接管要起整条 chat 线程或真实外部会话才能测,这里钉住它们取的是同一个解析函数。
    #[test]
    fn every_model_consumer_takes_the_chats_effective_value() {
        fn production(source: &str) -> &str {
            source.split("#[cfg(test)]\nmod tests").next().unwrap()
        }
        let chat_thread = production(include_str!("daemon/chat_thread.rs"));
        let turn_ctx = &chat_thread[chat_thread
            .find("let effective = crate::runtime::chat_model::effective(agent, Some(&tag));")
            .expect("每轮请求取本群生效值")..];
        let turn_ctx = &turn_ctx[..turn_ctx.find("codex_reasoning_summary").unwrap()];
        assert!(turn_ctx.contains("model: effective.model(),"));
        assert!(turn_ctx.contains("effort: effective.effort(),"));
        assert!(production(include_str!("../session/session.rs")).contains(
            "crate::runtime::chat_model::effective(self.agent, self.conversation_tag())"
        ));
        assert!(production(include_str!("../bridge/handoff.rs"))
            .contains("crate::runtime::chat_model::effective("));
        assert!(production(include_str!("../capture/rpc_pool.rs"))
            .contains("crate::capture::cursor_config::load_target(&fresh, &key.tag)"));
    }

    /// 反向守卫:取模型 / 档位一律经 [`effective`](要默认层就写 `effective(agent, None)`)。生产代码里
    /// 直接读 `agent.model()` / `agent.effort()` 的地方,就是没跟上本群覆盖的漏网点。不绑 chat 的
    /// workflow worker 与 debug 命令除外。
    ///
    /// 这是**按变量名的启发式**:只认 `RECEIVERS` 里这几个接收者名、只扫 `crates/summon/src`,把 Agent
    /// 存在别的名字里读就看不见。它挡住的是照旧习惯写出的漏网点,证明不了生产代码没有直接读 bot 默认。
    #[test]
    fn nothing_outside_chat_model_reads_the_bot_default_directly() {
        const EXEMPT: &[&str] = &[
            "runtime/chat_model.rs",
            "cli/dev.rs",
            "workflow/runner_transient.rs",
        ];
        const RECEIVERS: &[&str] = &["agent", "fresh", "fresh_agent", "current", "stale", "a"];
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut offenders = Vec::new();
        for file in files {
            let relative = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if EXEMPT.contains(&relative.as_str()) {
                continue;
            }
            let source = std::fs::read_to_string(&file).unwrap();
            // 跳过 `#[cfg(test)] mod … { … }`:rustfmt 后顶层模块的右括号独占一行、在第 0 列。
            let mut in_test = false;
            let mut pending_cfg_test = false;
            for (number, line) in source.lines().enumerate() {
                if in_test {
                    in_test = line != "}";
                    continue;
                }
                // 只有带 `{` 的内联模块才跳过;`mod tests;` 这类外部文件声明是单行,跳了会把后面
                // 一整段生产代码当成测试漏扫。
                if pending_cfg_test && line.starts_with("mod ") && line.trim_end().ends_with('{') {
                    in_test = true;
                    pending_cfg_test = false;
                    continue;
                }
                pending_cfg_test = line.trim() == "#[cfg(test)]";
                let code = line.split("//").next().unwrap_or_default();
                for receiver in RECEIVERS {
                    for call in [".model()", ".effort()"] {
                        let needle = format!("{receiver}{call}");
                        let hit = code.match_indices(&needle).any(|(at, _)| {
                            at == 0
                                || !code[..at]
                                    .chars()
                                    .last()
                                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
                        });
                        if hit {
                            offenders.push(format!("{relative}:{}: {}", number + 1, line.trim()));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "直接读 bot 默认的 model/effort,改经 chat_model::effective:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn a_hand_edited_override_that_is_not_a_model_id_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(tmp.path(), "name: examplebot\ncli: claude\nmodel: opus\n");
        let dir = chat_dir(&agent, "g-a").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model"), "把模型换成 sonnet\n").unwrap();
        assert_eq!(chat_model(&agent, "g-a"), None);
        assert_eq!(effective(&agent, Some("g-a")).model(), Some("opus"));
    }

    #[test]
    fn tags_that_could_leave_the_threads_directory_are_not_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_in(tmp.path(), "name: examplebot\ncli: claude\n");
        for tag in ["", "..", "g/../x", "g a", "."] {
            assert!(chat_dir(&agent, tag).is_none(), "{tag:?}");
            assert!(
                set_chat_model(&agent, tag, Some("opus")).is_err(),
                "{tag:?}"
            );
        }
        assert!(chat_dir(&agent, "g0123abcd").is_some());
    }
}
