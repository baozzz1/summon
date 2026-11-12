//! per-chat CLI session-id 记录的纯逻辑(字符串进/判定出):`.session` 文件读写格式、
//! codex rollout 转录的解析(id + cwd)。真正的文件 IO / 进程编排在 `summon` crate 的
//! `session.rs`。claude/cursor 的 id 由我们预先确定(无需解析);codex 的 id 由它自己生成,
//! 启动后回捞最新 rollout 转录拿到 —— 故这里给 codex 解析两件:会话 id 与其 cwd(多会话消歧)。

/// 一条 `.session` 记录:底层 CLI 种类 + 其原生 session id + **当前生效的工作目录**(可选,用于
/// 转录路径定位)。`cwd` 语义 = 会话「当前生效目录」:launch 时由 `effective_work_dir` 写入,
/// `/cd` 热切换会把它更新成新目录(见 `apply_chat_workdir`)。claude 的转录写在
/// `~/.claude/projects/<proj>/<sid>.jsonl`,`<proj>` 由该 cwd 推导。
///
/// 因为 `/cd` 只切 claude 内部 cwd、转录文件未必随之搬家(会话未重启),读取侧**不无条件采信**
/// 本字段:`resolve_transcript_path` 优先级 #1 会先验「cwd 绝对 ∧ 转录文件真在那儿」,不过则
/// 落到 SID 搜兜底自愈(C1/C6)。这也是本字段只允许绝对路径的原因(见 `format_record`)。
///
/// `Default` 是「最旧的那种记录」:除 cli/id 外一律无值。测试夹具与只关心一两个字段的构造点
/// 走 `..Default::default()`,这样再加字段时它们不必跟着改 —— 也不会因为漏改而编译不过。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRecord {
    pub cli: String,
    pub id: String,
    /// 当前生效的工作目录(**恒绝对路径**;非绝对值写入时被丢弃)。旧记录可能没有此字段(None),
    /// 此时读取侧回退 `effective_work_dir`。
    pub cwd: Option<String>,
    /// 本会话**首次冷启**的本地时间(`%Y-%m-%d %H:%M:%S`)。resume 不更新;reset 后新会话
    /// 冷启时重写。用于退出回执卡片展示「本会话起始时间」。
    pub created_at: Option<String>,
    /// Bridge RPC handoff 写下的严格恢复闸。为 true 时，读取侧只能恢复本记录的 exact id；
    /// provider 拒绝 resume 时不得 cold-start 并覆盖它。旧记录无此行，默认 false。
    pub strict_resume: bool,
    /// PRIMER 在 provider 侧占的槽位。**只对 codex/app-server 有意义**,其余通道恒 None。
    ///
    /// 存在的唯一理由是**存量迁移**:2026-08-21 之前建的 thread 把 PRIMER 写进了
    /// `ThreadStartParams.baseInstructions`,而那个字段是**替换**语义(替掉 codex 自己那份
    /// ~21KB 系统提示词)且**会随 rollout 持久化**——resume 时 codex 从 history 把它读回来,
    /// 谁也清不掉。之后建的 thread 改走 `developerInstructions`(**追加**语义,codex 自己的
    /// 提示词保留),但那个字段**不持久化**,每次 resume 必须重发。
    ///
    /// 两种 thread 的 resume 行为因此相反,而**光看 thread id 分不出来**:
    /// - `None`(旧记录 / 旧 thread):resume 不带 PRIMER —— 带了就会和 history 里那份撞成两遍。
    /// - `Some("developer")`:resume 必须带 `developerInstructions`,否则这个 thread 没有身份。
    ///
    /// 所以它不是缓存也不是优化位,是**判据**:漏了它,要么身份丢失,要么 PRIMER 翻倍。
    pub primer_slot: Option<String>,
    /// 这条会话的历史从哪个会话 fork 来的(跨群 fork 写下的血缘)。
    ///
    /// 排障时必须答得出「这个 sid 的历史哪来的」——它的转录里有一段本群没人说过的话,
    /// 没有这一行就只能靠猜。回执渲染与 reset 前的告知也读它。
    pub forked_from: Option<ForkOrigin>,
    /// fork 已落盘、但副本还没生出来:目标 chat 的**下一轮**要走 fork 形态启动,而不是冷启。
    ///
    /// 是**判据而非状态标记**:副本由 provider 在第一轮启动时生成(claude 的
    /// `--resume <父> --fork-session`、codex 的 `thread/fork`),那一轮成功后这一位必须清掉。
    /// 留着的话每一轮都会拿新 sid 再从父会话 fork 一次,每轮造一份新副本。
    pub fork_pending: bool,
}

/// 一条 fork 血缘:副本的历史取自哪个 chat 的哪个 session,以及取的是哪一刻的快照。
///
/// 三个字段都是排障必需:tag 定位源群、sid 定位源会话(源群可能已 reset 过好几轮)、
/// at 说明副本的截止点(源群在那之后说的话不在副本里)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkOrigin {
    /// 源 chat 的 tag(`chat_tag(chat_type, chat_id)`)。
    pub tag: String,
    /// 源会话的 provider 原生 session id。
    pub sid: String,
    /// fork 那一刻的本地时间(`%Y-%m-%d %H:%M:%S`)。
    pub at: String,
}

impl ForkOrigin {
    /// 渲染成 `.session` 的 `forked_from` 值:`<tag>:<sid>@<at>`。
    ///
    /// 三段都不允许含分隔符 —— tag 是 `[gp][0-9a-f]{16}`、sid 是 UUID 或 codex thread id、
    /// at 是定宽时间串,天然不含 `:` `@` 之外的歧义。构造侧用 [`ForkOrigin::new`] 把住。
    pub fn render(&self) -> String {
        format!("{}:{}@{}", self.tag, self.sid, self.at)
    }

    /// 从 `.session` 的 `forked_from` 值解析。任一段缺失或含空白即视为损坏 → None
    /// (血缘读不出来只丢失排障信息,不该让整条 `.session` 失效,故解析侧宽容)。
    ///
    /// 按**最后一个** `@` 切,再按**第一个** `:` 切:tag 里不会有 `:`,而时间串里不会有 `@`。
    pub fn parse(raw: &str) -> Option<Self> {
        let (head, at) = raw.rsplit_once('@')?;
        let (tag, sid) = head.split_once(':')?;
        Self::new(tag, sid, at)
    }

    /// 三段都非空、且都不含分隔符与空白才建得出来 —— 否则 render 出来的值 parse 不回原样。
    pub fn new(tag: &str, sid: &str, at: &str) -> Option<Self> {
        let tag = tag.trim();
        let sid = sid.trim();
        let at = at.trim();
        if tag.is_empty() || sid.is_empty() || at.is_empty() {
            return None;
        }
        // at 允许空格(时间串 `%Y-%m-%d %H:%M:%S` 中间就有一个),tag/sid 不允许。
        if tag.contains([':', '@']) || tag.contains(char::is_whitespace) {
            return None;
        }
        if sid.contains([':', '@']) || sid.contains(char::is_whitespace) {
            return None;
        }
        if at.contains('@') {
            return None;
        }
        Some(Self {
            tag: tag.to_string(),
            sid: sid.to_string(),
            at: at.to_string(),
        })
    }
}

/// [`SessionRecord::primer_slot`] 的唯一合法取值:PRIMER 走 `developerInstructions`。
/// 未来若再加槽位,判定点在 `thread_resume_params` 与 `start_codex`,别散开写字符串。
pub const PRIMER_SLOT_DEVELOPER: &str = "developer";

/// codex rollout 首行(session_meta)解析出的关键字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexMeta {
    pub id: String,
    pub cwd: Option<String>,
}

/// 渲染 `.session` 文件内容:`cli=<kind>`\n`id=<value>`\n,可选 `cwd=<path>`\n、`created_at=<time>`\n。
/// cwd 有值**且为绝对路径**才写出(向后兼容:旧记录无此行)。
/// created_at 有值才写出(向后兼容:旧记录无此行)。
///
/// **只落绝对路径**(C1):相对路径(字面 `.`、`./x`、`rel` 等)在读取侧 `project_dirname` 会被
/// 字面 munge 成垃圾目录(如 `.` → `-`,拼出 `~/.claude/projects/-/<sid>.jsonl`),转录永远
/// 定位不到。非绝对值当「无 cwd」丢弃,读取侧回退 `effective_work_dir` / SID 搜兜底。
pub fn format_record(cli: &str, id: &str, cwd: Option<&str>, created_at: Option<&str>) -> String {
    format_record_with_policy(cli, id, cwd, created_at, false, None, None, false)
}

/// 与 [`format_record`] 相同，额外落 `primer_slot`(见 [`SessionRecord::primer_slot`])。
/// 只有 codex 冷启那一处需要它,不把参数加进 [`format_record`] 是为了不动其余二十多个调用点。
pub fn format_record_with_primer_slot(
    cli: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
    primer_slot: Option<&str>,
) -> String {
    format_record_with_policy(cli, id, cwd, created_at, false, primer_slot, None, false)
}

/// 跨群 fork 落盘用:记录带 `forked_from` 血缘,并把 `fork_pending` 置起 —— 目标 chat 的
/// 下一轮据此走 fork 形态启动(见 [`SessionRecord::fork_pending`])。
///
/// `id` 是**目标会话的新 sid**,不是源 sid:两个 chat 共用一个 sid 就是双 writer。claude 的新
/// sid 由 summon 预铸,codex 的由 `thread/fork` 返回后回填。
pub fn format_fork_pending_record(
    cli: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
    origin: &ForkOrigin,
) -> String {
    format_record_with_policy(cli, id, cwd, created_at, false, None, Some(origin), true)
}

/// 与 [`format_record`] 相同，但把记录标成“只允许 exact resume”。该位跟随 `.session` 原子
/// move 穿过 Bridge handoff 提交点，避免另起 marker 文件造成两文件提交窗口。
pub fn format_strict_record(
    cli: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
) -> String {
    format_record_with_policy(cli, id, cwd, created_at, true, None, None, false)
}

/// 把一条已解析的记录原样渲染回去。
///
/// 供「读回记录 → 改一个字段 → 写回」的路径使用(改 cwd、清 strict 位)。这类路径必须走本函数,
/// 不能逐字段调 [`format_record`] 家族:那样每加一个字段,所有改写点都会静默把它丢掉 ——
/// 目标 chat 在 fork 后第一轮之前被 `/cd` 一下,`fork_pending` 就没了,fork 静默降级成冷启。
pub fn format_full_record(rec: &SessionRecord) -> String {
    format_record_with_policy(
        &rec.cli,
        &rec.id,
        rec.cwd.as_deref(),
        rec.created_at.as_deref(),
        rec.strict_resume,
        rec.primer_slot.as_deref(),
        rec.forked_from.as_ref(),
        rec.fork_pending,
    )
}

#[allow(clippy::too_many_arguments)]
fn format_record_with_policy(
    cli: &str,
    id: &str,
    cwd: Option<&str>,
    created_at: Option<&str>,
    strict_resume: bool,
    primer_slot: Option<&str>,
    forked_from: Option<&ForkOrigin>,
    fork_pending: bool,
) -> String {
    let mut out = format!("cli={cli}\nid={id}\n");
    if let Some(w) = cwd.filter(|w| w.starts_with('/')) {
        out.push_str(&format!("cwd={w}\n"));
    }
    if let Some(t) = created_at.filter(|t| !t.trim().is_empty()) {
        out.push_str(&format!("created_at={t}\n"));
    }
    if strict_resume {
        out.push_str("strict_resume=true\n");
    }
    // 空串按「没有」处理:与 cwd/created_at 同一条容忍规则,免得写出一行读回来是 None 的记录。
    if let Some(slot) = primer_slot.filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("primer_slot={slot}\n"));
    }
    if let Some(origin) = forked_from {
        out.push_str(&format!("forked_from={}\n", origin.render()));
    }
    if fork_pending {
        out.push_str("fork_pending=true\n");
    }
    out
}

/// 解析 `.session` 文件内容。`cli` 和 `id` 都齐且非空才算有效;否则 None(读到一半/损坏 → 当作无记录)。
/// `cwd` 可选,旧记录可能没有(解析出 None,调用方回退 `effective_work_dir`)。
/// `created_at` 可选,旧记录可能没有(解析出 None)。
pub fn parse_record(s: &str) -> Option<SessionRecord> {
    let mut cli = None;
    let mut id = None;
    let mut cwd = None;
    let mut created_at = None;
    let mut strict_resume = false;
    let mut primer_slot = None;
    let mut forked_from = None;
    let mut fork_pending = false;
    for line in s.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k.trim() {
                "cli" => cli = Some(v.to_string()),
                "id" => id = Some(v.to_string()),
                "cwd" => cwd = Some(v.to_string()),
                "created_at" => created_at = Some(v.to_string()),
                "primer_slot" => primer_slot = Some(v.to_string()),
                "forked_from" => forked_from = ForkOrigin::parse(v),
                "fork_pending" => match v {
                    "true" => fork_pending = true,
                    "false" => fork_pending = false,
                    _ => return None,
                },
                "strict_resume" => match v {
                    "true" => strict_resume = true,
                    "false" => strict_resume = false,
                    _ => return None,
                },
                _ => {}
            }
        }
    }
    Some(SessionRecord {
        cli: cli?,
        id: id?,
        cwd,
        created_at,
        strict_resume,
        primer_slot,
        forked_from,
        fork_pending,
    })
}

/// 解析 codex rollout 首行(`type":"session_meta"`)的 `payload.id` 与 `payload.cwd`。
/// 拿不到 id → None。比解析文件名更稳(直接信 codex 自己写的转录)。
pub fn codex_session_meta(first_line: &str) -> Option<CodexMeta> {
    let v: serde_json::Value = serde_json::from_str(first_line).ok()?;
    let payload = v.get("payload").unwrap_or(&v);
    let id = payload.get("id").and_then(|x| x.as_str())?.to_string();
    let cwd = payload
        .get("cwd")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    Some(CodexMeta { id, cwd })
}

/// 从 codex rollout 文件名兜底解析 session id(UUID)。
/// 形如 `rollout-2025-11-03T22-12-28-019a4a0f-ec62-76b2-84c2-8664368082e6.jsonl`:
/// 去掉扩展名后按 `-` 切,UUID 恒为末尾 5 段(8-4-4-4-12 hex)。首选 `codex_session_meta`,
/// 它读不到(转录首行损坏)时才用这个。
pub fn parse_codex_rollout_uuid(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".jsonl").unwrap_or(filename);
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let tail = &parts[parts.len() - 5..];
    let lens = [8usize, 4, 4, 4, 12];
    for (seg, want) in tail.iter().zip(lens.iter()) {
        if seg.len() != *want || !seg.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(tail.join("-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrips() {
        let doc = format_record("claude", "uuid-abc", None, None);
        assert_eq!(doc, "cli=claude\nid=uuid-abc\n");
        let r = parse_record(&doc).unwrap();
        assert_eq!(
            r,
            SessionRecord {
                cli: "claude".into(),
                id: "uuid-abc".into(),
                cwd: None,
                created_at: None,
                ..Default::default()
            }
        );
        // 带 cwd
        let doc2 = format_record("claude", "uuid-abc", Some("/Users/me/wf"), None);
        assert_eq!(doc2, "cli=claude\nid=uuid-abc\ncwd=/Users/me/wf\n");
        let r2 = parse_record(&doc2).unwrap();
        assert_eq!(
            r2,
            SessionRecord {
                cli: "claude".into(),
                id: "uuid-abc".into(),
                cwd: Some("/Users/me/wf".into()),
                created_at: None,
                ..Default::default()
            }
        );
        // 带 created_at
        let doc3 = format_record("claude", "uuid-abc", None, Some("2026-08-07 16:18:00"));
        assert_eq!(
            doc3,
            "cli=claude\nid=uuid-abc\ncreated_at=2026-08-07 16:18:00\n"
        );
        let r3 = parse_record(&doc3).unwrap();
        assert_eq!(
            r3,
            SessionRecord {
                cli: "claude".into(),
                id: "uuid-abc".into(),
                cwd: None,
                created_at: Some("2026-08-07 16:18:00".into()),
                ..Default::default()
            }
        );
        // 带 cwd + created_at
        let doc4 = format_record(
            "claude",
            "uuid-abc",
            Some("/Users/me/wf"),
            Some("2026-08-07 16:18:00"),
        );
        assert_eq!(
            doc4,
            "cli=claude\nid=uuid-abc\ncwd=/Users/me/wf\ncreated_at=2026-08-07 16:18:00\n"
        );
        let r4 = parse_record(&doc4).unwrap();
        assert_eq!(
            r4,
            SessionRecord {
                cli: "claude".into(),
                id: "uuid-abc".into(),
                cwd: Some("/Users/me/wf".into()),
                created_at: Some("2026-08-07 16:18:00".into()),
                ..Default::default()
            }
        );

        let strict = format_strict_record("codex", "thread-1", Some("/work"), None);
        assert!(strict.ends_with("strict_resume=true\n"));
        assert!(parse_record(&strict).unwrap().strict_resume);
    }

    #[test]
    fn format_record_drops_non_absolute_cwd() {
        // C1 回归:相对路径(字面 "." / "./x" / "rel" 等)绝不能落进 .session。读取侧
        // project_dirname(".")="-" 会 munge 出 ~/.claude/projects/-/<sid>.jsonl 垃圾路径,
        // 转录永远定位不到(生产失明)。写入侧只落**绝对路径**,非绝对当「无 cwd」丢弃,
        // 读取侧回退 effective_work_dir / SID 搜兜底。
        assert_eq!(
            format_record("claude", "id", Some("."), None),
            "cli=claude\nid=id\n"
        );
        assert_eq!(
            format_record("claude", "id", Some("./sub"), None),
            "cli=claude\nid=id\n"
        );
        assert_eq!(
            format_record("claude", "id", Some("relative/path"), None),
            "cli=claude\nid=id\n"
        );
        // 绝对路径正常写出
        assert_eq!(
            format_record("claude", "id", Some("/abs"), None),
            "cli=claude\nid=id\ncwd=/abs\n"
        );
        // 空串仍丢弃(向后兼容原行为)
        assert_eq!(
            format_record("claude", "id", Some(""), None),
            "cli=claude\nid=id\n"
        );
    }

    #[test]
    fn format_record_drops_empty_created_at() {
        // 空 / 纯空白 created_at 不落行(向后兼容)。
        assert_eq!(
            format_record("claude", "id", None, Some("")),
            "cli=claude\nid=id\n"
        );
        assert_eq!(
            format_record("claude", "id", None, Some("   ")),
            "cli=claude\nid=id\n"
        );
        assert_eq!(
            format_record("claude", "id", None, Some("2026-08-07 16:18:00")),
            "cli=claude\nid=id\ncreated_at=2026-08-07 16:18:00\n"
        );
    }

    #[test]
    fn parse_record_needs_both_fields_nonempty() {
        assert!(parse_record("cli=claude\n").is_none()); // 缺 id
        assert!(parse_record("id=x\n").is_none()); // 缺 cli
        assert!(parse_record("cli=claude\nid=\n").is_none()); // id 空
        assert!(parse_record("").is_none());
        // 容忍多余行 / 空白;cwd 空行不影响(被跳过)
        let r = parse_record("  cli = cursor \n id = chat1 \nfoo=bar\ncwd=\n").unwrap();
        assert_eq!(r.cli, "cursor");
        assert_eq!(r.id, "chat1");
        assert_eq!(r.cwd, None); // cwd 空 → None
        assert_eq!(r.created_at, None); // 无 created_at 行 → None
                                        // cwd 有值 → 解析出
        let r2 = parse_record("cli=claude\nid=abc\ncwd=/tmp\n").unwrap();
        assert_eq!(r2.cwd.as_deref(), Some("/tmp"));
        // created_at 有值 → 解析出
        let r3 = parse_record("cli=claude\nid=abc\ncreated_at=2026-08-07 16:18:00\n").unwrap();
        assert_eq!(r3.created_at.as_deref(), Some("2026-08-07 16:18:00"));
    }

    #[test]
    fn codex_meta_reads_id_and_cwd_from_payload() {
        let line = r#"{"timestamp":"t","type":"session_meta","payload":{"id":"019ab904-92e2-73b0-a4ad-69ee97d72e7f","cwd":"/Users/me/wf","cli_version":"0.61"}}"#;
        let m = codex_session_meta(line).unwrap();
        assert_eq!(m.id, "019ab904-92e2-73b0-a4ad-69ee97d72e7f");
        assert_eq!(m.cwd.as_deref(), Some("/Users/me/wf"));
    }

    #[test]
    fn codex_meta_none_without_id() {
        assert!(codex_session_meta("not json").is_none());
        assert!(codex_session_meta(r#"{"type":"x","payload":{"cwd":"/a"}}"#).is_none());
        // payload 缺失时也能从顶层兜底
        let m = codex_session_meta(r#"{"id":"abc","cwd":"/a"}"#).unwrap();
        assert_eq!(m.id, "abc");
    }

    #[test]
    fn rollout_filename_uuid_is_last_five_groups() {
        let name = "rollout-2025-11-03T22-12-28-019a4a0f-ec62-76b2-84c2-8664368082e6.jsonl";
        assert_eq!(
            parse_codex_rollout_uuid(name).as_deref(),
            Some("019a4a0f-ec62-76b2-84c2-8664368082e6")
        );
        // 非法 / 太短 → None
        assert!(parse_codex_rollout_uuid("rollout-foo.jsonl").is_none());
        assert!(parse_codex_rollout_uuid("019a4a0f-ec62-76b2-84c2-nothex000000.jsonl").is_none());
    }
}

#[cfg(test)]
mod primer_slot_tests {
    use super::*;

    #[test]
    fn primer_slot_round_trips_through_format_and_parse() {
        let legacy = parse_record("cli=codex\nid=th-1\n").expect("旧 Codex 记录必须仍可解析");
        assert_eq!(legacy.primer_slot, None);
        let text = format_record_with_primer_slot(
            "codex",
            "th-1",
            Some("/tmp/work"),
            Some("2026-08-21 10:00:00"),
            Some(PRIMER_SLOT_DEVELOPER),
        );
        let record = parse_record(&text).expect("刚写出的记录必须解析得回来");
        assert_eq!(record.primer_slot.as_deref(), Some(PRIMER_SLOT_DEVELOPER));
        assert_eq!(record.cli, "codex");
        assert_eq!(record.id, "th-1");
        assert_eq!(record.cwd.as_deref(), Some("/tmp/work"));
    }

    #[test]
    fn an_empty_primer_slot_is_dropped_rather_than_written_back_as_none() {
        // 写出一行 `primer_slot=` 再读回来会是 None,那就是一条自相矛盾的记录。
        let text = format_record_with_primer_slot("codex", "th-1", None, None, Some("  "));
        assert!(
            !text.contains("primer_slot"),
            "空槽位不该落盘,实际写出:{text:?}"
        );
    }

    #[test]
    fn format_record_stays_legacy_so_existing_call_sites_do_not_silently_opt_in() {
        // 二十多个调用点仍走 `format_record`。它必须继续写「没有槽位」的旧形态,
        // 否则 cursor / claude 的记录会凭空多出一个只对 codex 有意义的判据。
        let text = format_record("cursor-acp", "sess-1", None, None);
        assert!(!text.contains("primer_slot"), "实际写出:{text:?}");
    }

    #[test]
    fn an_unknown_future_key_is_ignored_instead_of_failing_the_record() {
        // 反向兼容:装了新版 summon 的机器写出的记录,回滚到旧版也得读得动。
        let record = parse_record("cli=codex\nid=th-1\nsome_future_key=x\nprimer_slot=developer\n")
            .expect("未知键不该让整条记录作废");
        assert_eq!(record.primer_slot.as_deref(), Some(PRIMER_SLOT_DEVELOPER));
    }

    fn origin() -> ForkOrigin {
        ForkOrigin::new("g00000000a11ce04d", "src-sid-1", "2026-09-03 11:52:30")
            .expect("样例血缘应当合法")
    }

    #[test]
    fn a_fork_origin_rejects_segments_that_would_not_round_trip() {
        // tag/sid 含分隔符会让 render 出的值切错段,parse 回来就是另一条血缘 —— 构造侧就拒。
        assert!(ForkOrigin::new("tag:x", "sid", "2026-09-03 11:52:30").is_none());
        assert!(ForkOrigin::new("tag", "sid@y", "2026-09-03 11:52:30").is_none());
        assert!(ForkOrigin::new("tag", "s id", "2026-09-03 11:52:30").is_none());
        assert!(ForkOrigin::new("", "sid", "t").is_none());
        assert!(ForkOrigin::new("tag", "", "t").is_none());
        assert!(ForkOrigin::new("tag", "sid", " ").is_none());
    }

    #[test]
    fn a_fork_origin_keeps_the_space_inside_a_timestamp() {
        // at 是 `%Y-%m-%d %H:%M:%S`,中间那个空格必须留着 —— 按空白拒会把全部合法血缘拒掉。
        let parsed = ForkOrigin::parse("gtag:sid@2026-09-03 11:52:30").expect("时间串合法");
        assert_eq!(parsed.at, "2026-09-03 11:52:30");
    }

    #[test]
    fn a_fork_pending_record_survives_a_round_trip() {
        let text = format_fork_pending_record(
            "claude",
            "new-sid-1",
            Some("/repo"),
            Some("2026-09-03 11:52:31"),
            &origin(),
        );
        let record = parse_record(&text).expect("fork 记录必须解析得动");
        assert_eq!(record.id, "new-sid-1");
        assert!(record.fork_pending);
        assert_eq!(record.forked_from, Some(origin()));
        // fork 与 strict 是两条独立的闸,fork 落盘不该顺带把 exact-resume 锁上。
        assert!(!record.strict_resume);
    }

    #[test]
    fn a_full_record_round_trip_keeps_every_field() {
        // 「读回记录 → 改一个字段 → 写回」的路径全走 `format_full_record`。它漏掉任何字段,
        // 都会让一次 `/cd` 静默清掉 fork 判据,fork 降级成冷启且无人察觉。
        let original = SessionRecord {
            cli: "codex".into(),
            id: "th-9".into(),
            cwd: Some("/repo".into()),
            created_at: Some("2026-09-03 11:52:31".into()),
            strict_resume: true,
            primer_slot: Some(PRIMER_SLOT_DEVELOPER.into()),
            forked_from: Some(origin()),
            fork_pending: true,
        };
        let parsed = parse_record(&format_full_record(&original)).expect("整记录必须解析得动");
        assert_eq!(parsed, original);
    }

    #[test]
    fn the_legacy_formatters_never_opt_a_record_into_fork() {
        // 二十多个既有调用点仍走这几个入口。它们凭空写出 fork 判据的话,
        // 普通冷启会被下一轮当成「该 fork 了」,从一个不存在的父会话取历史。
        for text in [
            format_record("claude", "sid-1", None, None),
            format_record_with_primer_slot(
                "codex",
                "th-1",
                None,
                None,
                Some(PRIMER_SLOT_DEVELOPER),
            ),
            format_strict_record("claude", "sid-1", None, None),
        ] {
            assert!(!text.contains("fork_pending"), "实际写出:{text:?}");
            assert!(!text.contains("forked_from"), "实际写出:{text:?}");
        }
    }

    #[test]
    fn a_corrupt_forked_from_line_does_not_void_the_whole_record() {
        // 血缘只服务排障。它读不出来时丢掉血缘、保住会话;让整条 `.session` 失效会把一个
        // 还能正常 resume 的会话打成「无记录」,下一轮冷启覆盖掉真实历史。
        let record = parse_record("cli=claude\nid=sid-1\nforked_from=没有分隔符\n")
            .expect("坏血缘不该让整条记录作废");
        assert_eq!(record.id, "sid-1");
        assert_eq!(record.forked_from, None);
    }

    #[test]
    fn a_malformed_fork_pending_value_voids_the_record() {
        // 与 `strict_resume` 同一条纪律:判据位读不准就必须 fail-closed。把 `fork_pending=maybe`
        // 当成 false,会让一次该 fork 的启动静默冷启,目标群拿到一个空会话还以为带着上下文。
        assert!(parse_record("cli=claude\nid=sid-1\nfork_pending=maybe\n").is_none());
    }
}
