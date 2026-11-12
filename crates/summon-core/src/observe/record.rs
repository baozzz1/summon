//! cassette 纯模型(录制-回放夹具):无副作用的 serde 数据类型与白名单/转换纯逻辑。
//!
//! 这里只放「数据形状 + 纯函数」(可单测、不碰 IO/进程):
//! - `SpawnSpecRecord` 是 `runtime::executor::SpawnSpec` 的 serde 镜像(`SpawnSpec` 含
//!   `own_process_group`/`stdin_null` 等运行期语义、不便直接 derive serde,故另立镜像)。
//! - `CassetteTurn` 把一轮无头子进程的「起进程描述 + stdout 原始字节文件名」打包,落 `spawn.json`。
//! - `is_safe_env_key` 是录制脱敏的第一道闸:只放行 `SUMMON_*` 白名单,丢 `FEISHU_*`/`*_SECRET`/
//!   `*_TOKEN` 等凭据键——录制时即不写入 raw(promote 再兜底 scrub)。
//!
//! IO(落盘/读回)在 `summon::runtime::record_store`;此处恒驻(纯逻辑),由调用方在 debug 构建里用。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// `SpawnSpec` 的 serde 镜像:只留可序列化、回放需要的字段。`env` 用 `Vec<(String,String)>`(保
/// 顺序、与 `SpawnSpec.env` 同形),录制前经 `is_safe_env_key` 过滤掉凭据键。`stdin_null` 透传以便
/// 回放比对(回放本身 spawn `cat`,不真起 codex,但 spec 比对要逐字段对齐)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpecRecord {
    pub program: String,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
    #[serde(default)]
    pub stdin_null: bool,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// 一轮无头子进程的 cassette 记录:起进程描述 + stdout 原始字节的相对文件名(默认 `stdout.raw`)。
/// 落 `turns/turn-<n>/spawn.json`;`stdout_file` 指向同目录下的原始字节文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CassetteTurn {
    pub spec: SpawnSpecRecord,
    pub stdout_file: String,
}

/// 录制 env 白名单:只放行 `SUMMON_` 前缀键。其余(尤其 `FEISHU_*`/`*_SECRET`/`*_TOKEN`/`*_KEY`
/// 等凭据)一律丢——录制时即不写入 raw,从源头避免密钥落盘。判定纯大小写敏感前缀匹配,简单可测。
pub fn is_safe_env_key(key: &str) -> bool {
    key.starts_with("SUMMON_")
}

/// 会话文件快照的「按需定位」纯逻辑(RR3.1):给定 CLI 与本轮 session_id,判定磁盘上哪些会话文件
/// 属于这一轮——只拷该轮真读写的文件,绝不整盘拷 `~/.claude`/`~/.cursor`/`~/.codex`。
///
/// 三类 CLI 的会话文件命名各异、可定位性不同(实测磁盘):
/// - **claude**:`~/.claude/projects/<project_dirname(cwd)>/<sid>.jsonl`,由 cwd+sid **确定性**拼出
///   (见 `claude_log::claude_transcript_path`,IO 在 summon::capture::transcript)。
/// - **codex**:`~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<sid>.jsonl`,日期分桶 + 文件名**内嵌**
///   sid,故只能按「文件名含该 sid」扫描匹配(`codex_rollout_matches_sid`)。
/// - **cursor**:`~/.cursor/chats/<chat-hex>/…`,chat-hex 非 resume sid,故无确定性映射 —— 录制侧只能
///   退化为「按 mtime 取该轮窗口内新写的 chat 目录」近似(IO 决策,本模块只给纯判定原语)。
///
/// 这里只放**纯判定**(无 IO):文件名是否归属某 sid。实际扫描/拷贝在 summon 侧(注入 home 根、用
/// 这些判定筛选要拷的文件),回放挂载时把 home 指向快照(RR3.2)。
///
/// codex rollout 文件名是否归属 `session_id`:命名形如 `rollout-<ts>-<sid>.jsonl`,sid 是 UUID 形态
/// 的末段。判定「文件名(不含目录)是否以 `.jsonl` 结尾且**包含**该 sid」——sid 唯一,子串命中即归属。
pub fn codex_rollout_matches_sid(file_name: &str, session_id: &str) -> bool {
    !session_id.is_empty() && file_name.ends_with(".jsonl") && file_name.contains(session_id)
}

/// claude 转录文件名是否归属 `session_id`:claude 命名恰为 `<sid>.jsonl`,故文件名须**恰等于**
/// `<sid>.jsonl`(不同于 codex 的子串包含——claude 一文件一 sid、精确匹配避免误拷邻轮)。
pub fn claude_transcript_matches_sid(file_name: &str, session_id: &str) -> bool {
    !session_id.is_empty() && file_name == format!("{session_id}.jsonl")
}

/// 脱敏(scrub,RR4.4):promote 把 `~/.summon/raw/` 的忠实全量录制脱敏成可入仓 fixture 时,逐字符串
/// 跑这套**纯**替换。**铁律**:替换必须**保回放语义**——同一 open_id 恒映射同一稳定别名(保 actor
/// 关系不漂移)、结构不变(只换值不换形状)。promote 拷文件前对每个 JSON 字符串字段跑 `scrub_text`。
///
/// 稳定别名分配器:open_id(`ou_<hex>`)→ `ou_alice`/`ou_bob`/…(按首现序,同 id 同名)。promote 一次
/// 持一个分配器贯穿整个场景,保证跨文件(inbound/outbound)同一 open_id 得同一别名。
#[derive(Default)]
pub struct AliasMap {
    map: HashMap<String, String>,
    next: usize,
}

/// 稳定别名表(按序):alice, bob, carol, …;超出后退化为 `ou_user<n>`。覆盖典型录制(参与者少)。
const ALIAS_NAMES: &[&str] = &[
    "alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi",
];

impl AliasMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取(或首次分配)某 open_id 的稳定别名 `ou_<name>`。同 id 同名(保 actor 关系)。
    fn alias_for(&mut self, open_id: &str) -> String {
        if let Some(a) = self.map.get(open_id) {
            return a.clone();
        }
        let name = ALIAS_NAMES
            .get(self.next)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("user{}", self.next));
        let alias = format!("ou_{name}");
        self.next += 1;
        self.map.insert(open_id.to_string(), alias.clone());
        alias
    }
}

/// 某字符串是否像飞书 open_id(`ou_` + 一串 hex/字母数字)。稳定别名本身(`ou_alice`)不可再被当
/// open_id 替换 —— 故要求 `ou_` 后**全是 hex**(真 open_id 是 hex),别名是字母词,不命中。
fn looks_like_open_id(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix("ou_") {
        rest.len() >= 8 && rest.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        false
    }
}

/// 对一段文本做脱敏替换(纯):
/// 1. 把其中所有 open_id(`ou_<hex>`)替换成稳定别名(`ou_alice` 等;经 `aliases` 保跨调用稳定)。
/// 2. `redact_content=true` 时,**额外**把非 open_id 的正文整体替换成 `[redacted]`(强脱敏:连业务
///    正文都不入仓;默认 false,只脱 open_id,保回放语义最大)。
///
/// open_id 用「token 切分 + 整 token 命中」而非正则:按非[字母数字下划线]切词,逐 token 若
/// `looks_like_open_id` 则换别名,其余原样拼回。简单、可测、不引正则依赖。
pub fn scrub_text(text: &str, aliases: &mut AliasMap, redact_content: bool) -> String {
    // 先做 open_id → 别名(永远做,保 actor 关系)。
    let replaced = replace_open_ids(text, aliases);
    if redact_content {
        // 强脱敏:正文整体抹掉(open_id 已先换成别名,别名是结构关系、保留;其余正文→[redacted])。
        // 简化策略:整段非空白若不等于纯别名,则抹成 [redacted]。这里按「含别名则保、否则抹」过粗,
        // 故采用更安全的逐 token:别名 token 保留、其余非空白 token → [redacted](去重相邻)。
        return redact_non_alias_tokens(&replaced, aliases);
    }
    replaced
}

/// 把文本里的 open_id token 换成稳定别名,其余 token 原样保留(保结构/空白)。
fn replace_open_ids(text: &str, aliases: &mut AliasMap) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String, aliases: &mut AliasMap| {
        if !token.is_empty() {
            if looks_like_open_id(token) {
                out.push_str(&aliases.alias_for(token));
            } else {
                out.push_str(token);
            }
            token.clear();
        }
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            flush(&mut token, &mut out, aliases);
            out.push(ch);
        }
    }
    flush(&mut token, &mut out, aliases);
    out
}

/// 强脱敏:别名 token(`ou_alice` 等已分配的别名)保留,其余非空白 token 抹成 `[redacted]`(相邻
/// 抹除合并成一个,避免 `[redacted] [redacted]` 噪音)。保留分隔符让结构仍可读。
fn redact_non_alias_tokens(text: &str, aliases: &AliasMap) -> String {
    let alias_set: std::collections::HashSet<&str> =
        aliases.map.values().map(|s| s.as_str()).collect();
    let mut out = String::new();
    let mut last_was_redacted = false;
    for tok in text.split_inclusive(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        // 拆出 token 主体 + 尾随分隔符。
        let split = tok
            .char_indices()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
            .map(|(i, _)| i)
            .unwrap_or(tok.len());
        let (word, sep) = tok.split_at(split);
        if word.is_empty() {
            out.push_str(sep);
            last_was_redacted = false;
        } else if alias_set.contains(word) {
            out.push_str(word);
            out.push_str(sep);
            last_was_redacted = false;
        } else {
            if !last_was_redacted {
                out.push_str("[redacted]");
            }
            out.push_str(sep);
            last_was_redacted = true;
        }
    }
    out
}

/// scenario 索引(RR4.1):把一次交互的原子记录(入站 / 无头轮 / 出站 / 会话快照)按 `chat_id`
/// 串成一条端到端流的元数据。落 `<scenario>/scenario.json`,回放 harness 据它知道「这条场景有几轮、
/// 哪个 cli、出/入站各几条、会话快照在哪」。**纯数据 + 纯 correlation**(无 IO),IO 在 record_store。
///
/// 设计取舍:原子记录已各自落盘(`inbound.ndjson`/`turns/turn-<n>/`/`outbound.ndjson`/`session/`),
/// 索引**不复制内容**,只记每侧的计数 + 顺序关联键(chat_id + 到达序),回放据此按序读回各侧文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioIndex {
    /// 场景名(= raw 子目录名)。
    pub name: String,
    /// 该场景的 CLI(claude/cursor/codex;决定回放挂会话快照的子树与解析层)。
    pub cli: String,
    /// 串成 e2e 流的各 chat(按 chat_id 分组、各自保到达序)。通常一条场景只 1 个 chat。
    pub flows: Vec<ChatFlow>,
}

/// 一条 chat 的端到端流:该 chat 下按到达序排列的原子记录引用(入站第几条 / 第几轮 / 出站第几条)。
/// 回放注入器据此「先注入入站[i] → 跑 turn[j] → 断言出站[k]」复现真实时序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatFlow {
    pub chat_id: String,
    /// 该 chat 的事件序列(到达序),每项指向某侧原子记录的下标。
    pub steps: Vec<FlowStep>,
}

/// e2e 流里的一步:指向某侧原子记录(按其在该侧 ndjson / turns 目录中的下标)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowStep {
    /// `inbound.ndjson` 第 `index` 条入站(消息 / 卡片回调)。
    Inbound { index: usize },
    /// `turns/turn-<index>/` 第 `index` 轮无头子进程。
    Turn { index: usize },
    /// `outbound.ndjson` 第 `index` 条出站(Frontend 调用)。
    Outbound { index: usize },
}

/// 录制侧一条原子记录的最小关联视图(correlation 输入):它属于哪个 chat、是哪一侧、在该侧的下标。
/// 录制点按到达序逐条 push(`seq` = 全局到达序,用于跨侧排序),correlation 据此分组 + 排序。
/// serde:录制时 append 进 `timeline.ndjson`,promote 时读回喂 `correlate` 生成 `scenario.json`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomRef {
    pub chat_id: String,
    /// 全局到达序(单调递增):跨侧把入站/轮/出站按真实时序穿成一条流。
    pub seq: u64,
    pub step: FlowStep,
}

/// correlation 纯逻辑(RR4.1):把按到达序收集的原子记录引用,按 `chat_id` 分组、组内按 `seq` 排序,
/// 串成每 chat 一条 `ChatFlow`。chat 出现顺序 = 各 chat **首条** record 的到达序(稳定、可复现)。
///
/// 不做去重 / 不校验配对(录制忠实即可):同一 chat 的多轮/多出站全保留,回放按 steps 顺序重放。
pub fn correlate(name: &str, cli: &str, mut atoms: Vec<AtomRef>) -> ScenarioIndex {
    // 先按全局到达序排序(稳定),保证组内 steps 与 chat 首现序都遵循真实时序。
    atoms.sort_by_key(|a| a.seq);
    let mut order: Vec<String> = Vec::new();
    let mut by_chat: std::collections::HashMap<String, Vec<FlowStep>> =
        std::collections::HashMap::new();
    for a in atoms {
        if !by_chat.contains_key(&a.chat_id) {
            order.push(a.chat_id.clone());
        }
        by_chat.entry(a.chat_id).or_default().push(a.step);
    }
    let flows = order
        .into_iter()
        .map(|chat_id| {
            let steps = by_chat.remove(&chat_id).unwrap_or_default();
            ChatFlow { chat_id, steps }
        })
        .collect();
    ScenarioIndex {
        name: name.to_string(),
        cli: cli.to_string(),
        flows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_rollout_matches_sid_by_substring() {
        let sid = "019e6224-f982-7cb0-a2ea-6701ba379e31";
        let fname = format!("rollout-2026-05-26T10-37-30-{sid}.jsonl");
        assert!(codex_rollout_matches_sid(&fname, sid));
        // 别的 sid 不命中。
        assert!(!codex_rollout_matches_sid(&fname, "deadbeef-0000"));
        // 非 jsonl(session_index 等)不命中。
        assert!(!codex_rollout_matches_sid(
            &format!("rollout-x-{sid}.log"),
            sid
        ));
        // 空 sid 永不命中(避免误拷整盘)。
        assert!(!codex_rollout_matches_sid(&fname, ""));
    }

    #[test]
    fn claude_transcript_matches_sid_exactly() {
        let sid = "07778859-d337-4a01-a41d-e4c11ae7be81";
        assert!(claude_transcript_matches_sid(&format!("{sid}.jsonl"), sid));
        // 仅子串包含不够(claude 一文件一 sid,须恰等)。
        assert!(!claude_transcript_matches_sid(
            &format!("prefix-{sid}.jsonl"),
            sid
        ));
        assert!(!claude_transcript_matches_sid(&format!("{sid}.log"), sid));
        assert!(!claude_transcript_matches_sid("x.jsonl", ""));
    }

    #[test]
    fn correlate_groups_by_chat_and_orders_by_seq() {
        // 两个 chat 交错到达:correlate 应按 chat 分组、组内按 seq 排序,chat 顺序 = 各自首现序。
        let atoms = vec![
            AtomRef {
                chat_id: "oc_a".into(),
                seq: 0,
                step: FlowStep::Inbound { index: 0 },
            },
            AtomRef {
                chat_id: "oc_b".into(),
                seq: 1,
                step: FlowStep::Inbound { index: 1 },
            },
            AtomRef {
                chat_id: "oc_a".into(),
                seq: 2,
                step: FlowStep::Turn { index: 0 },
            },
            AtomRef {
                chat_id: "oc_a".into(),
                seq: 3,
                step: FlowStep::Outbound { index: 0 },
            },
            AtomRef {
                chat_id: "oc_b".into(),
                seq: 4,
                step: FlowStep::Outbound { index: 1 },
            },
        ];
        let idx = correlate("scn", "cursor", atoms);
        assert_eq!(idx.name, "scn");
        assert_eq!(idx.cli, "cursor");
        // chat 顺序 = 首现序:oc_a 先(seq 0)、oc_b 后(seq 1)。
        assert_eq!(idx.flows.len(), 2);
        assert_eq!(idx.flows[0].chat_id, "oc_a");
        assert_eq!(idx.flows[1].chat_id, "oc_b");
        // oc_a 组内按 seq:inbound→turn→outbound。
        assert_eq!(
            idx.flows[0].steps,
            vec![
                FlowStep::Inbound { index: 0 },
                FlowStep::Turn { index: 0 },
                FlowStep::Outbound { index: 0 },
            ]
        );
        assert_eq!(
            idx.flows[1].steps,
            vec![
                FlowStep::Inbound { index: 1 },
                FlowStep::Outbound { index: 1 },
            ]
        );
    }

    #[test]
    fn correlate_sorts_out_of_order_seq() {
        // 乱序到达(seq 非递增传入):correlate 先全局排序,组内仍按真实时序。
        let atoms = vec![
            AtomRef {
                chat_id: "oc_a".into(),
                seq: 5,
                step: FlowStep::Outbound { index: 0 },
            },
            AtomRef {
                chat_id: "oc_a".into(),
                seq: 1,
                step: FlowStep::Inbound { index: 0 },
            },
        ];
        let idx = correlate("scn", "codex", atoms);
        assert_eq!(
            idx.flows[0].steps,
            vec![
                FlowStep::Inbound { index: 0 },
                FlowStep::Outbound { index: 0 },
            ]
        );
    }

    #[test]
    fn scenario_index_serde_round_trip() {
        let idx = ScenarioIndex {
            name: "scn".into(),
            cli: "claude".into(),
            flows: vec![ChatFlow {
                chat_id: "oc_a".into(),
                steps: vec![FlowStep::Inbound { index: 0 }, FlowStep::Turn { index: 0 }],
            }],
        };
        let json = serde_json::to_string(&idx).unwrap();
        let back: ScenarioIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(idx, back);
    }

    #[test]
    fn scrub_maps_open_id_to_stable_alias_same_id_same_name() {
        let mut aliases = AliasMap::new();
        let a = "ou_1a2b3c4d5e6f7a8b";
        let b = "ou_ffffeeeeddddcccc";
        // 同一 id 两处出现得同一别名;不同 id 得不同别名(保 actor 关系)。
        let s1 = scrub_text(&format!("from {a} to {b}"), &mut aliases, false);
        let s2 = scrub_text(&format!("again {a}"), &mut aliases, false);
        assert_eq!(s1, "from ou_alice to ou_bob");
        assert_eq!(s2, "again ou_alice", "同 id 跨调用必须同别名");
        // 别名本身不会被二次替换(ou_alice 非 hex)。
        let s3 = scrub_text("ou_alice stays", &mut aliases, false);
        assert_eq!(s3, "ou_alice stays");
    }

    #[test]
    fn scrub_preserves_structure_only_swaps_open_ids() {
        let mut aliases = AliasMap::new();
        let json = r#"{"sender":"ou_aabbccddeeff0011","text":"hi"}"#;
        let out = scrub_text(json, &mut aliases, false);
        // 结构(引号/冒号/花括号)不变,只 open_id 换别名;正文 hi 保留(非强脱敏)。
        assert_eq!(out, r#"{"sender":"ou_alice","text":"hi"}"#);
    }

    #[test]
    fn scrub_redact_content_keeps_aliases_masks_rest() {
        let mut aliases = AliasMap::new();
        // 先让 open_id 分配别名,再强脱敏:别名保留、其余正文 →[redacted]。
        let out = scrub_text(
            "ou_aabbccddeeff0011 said secret-plan details",
            &mut aliases,
            true,
        );
        assert!(out.contains("ou_alice"), "别名须保留(actor 关系)");
        assert!(out.contains("[redacted]"), "正文须被抹");
        assert!(!out.contains("secret-plan"), "敏感正文不得入仓: {out}");
    }

    #[test]
    fn looks_like_open_id_distinguishes_alias_from_real() {
        assert!(looks_like_open_id("ou_aabbccddeeff0011"));
        // 别名(字母词)不是 open_id,不会被再替换。
        assert!(!looks_like_open_id("ou_alice"));
        // 太短 / 无前缀。
        assert!(!looks_like_open_id("ou_aabb"));
        assert!(!looks_like_open_id("oc_chat_id"));
    }

    #[test]
    fn is_safe_env_key_allows_summon_drops_secrets() {
        assert!(is_safe_env_key("SUMMON_CHAT_ID"));
        assert!(is_safe_env_key("SUMMON_CONVERSATION_TAG"));
        assert!(!is_safe_env_key("FEISHU_APP_SECRET"));
        assert!(!is_safe_env_key("OPENAI_API_KEY"));
        assert!(!is_safe_env_key("SOME_TOKEN"));
        assert!(!is_safe_env_key("HOME"));
    }
}
