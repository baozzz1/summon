//! YAML 原文模式的保存前校验:解析错误(带行列)、严格子结构的未知键、语义非法值记为错误;
//! 其余未知键(附最接近的合法键)与废弃键记为警告;外部读取的键不报。另给出改动前后的统一差异。
//!
//! 未知键的判据来自结构体本身:serde 派生的 `Deserialize` 会把字段名清单交给
//! `deserialize_struct`,这里截下那份清单([`fields_of`]),所以结构体加了字段这里自动认得,
//! 不维护第二份键名表。

use super::fields::{key_note, NoteKind, KEY_NOTES};
use super::RawKind;
use crate::config::codex_policy::{self, CodexPolicyOverride, APPROVAL_VALUES, SANDBOX_VALUES};
use crate::config::config::{
    path_exists_in_value, Claude, Config, Cursor, CursorQuota, Dashboard, GlobalFeishu, MenuBar,
    MessagingOverride, PreviewServer, Remote, Sentinel, SessionExitedOverride, Tools, Upgrade,
    WebTerminal, CONFIG_FIELD_META,
};
use crate::config::menubar::MENUBAR_ICONS;
use crate::config::model_effort;
use crate::config::registry::{Agent, CaptureOverride, Feishu, Runtime};
use crate::driver::slash_pool::SlashBlacklistOverride;
use crate::driver::{reasoning_summary_decision, CliDriver};
use serde::de::{DeserializeOwned, Visitor};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RawIssue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RawCheck {
    /// 统一差异格式;两边相同时为空串。
    pub diff: String,
    pub errors: Vec<RawIssue>,
    pub warnings: Vec<RawIssue>,
}

impl RawCheck {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// 校验 `new`(要保存的全文)。`old` 是当前盘上的全文,用来出差异,并区分「这次改出来的问题」
/// 与「原本就有的问题」:语义非法的值只有在这次改动里变了才算错误,没动过的降为警告 ——
/// 否则一处与本次无关的旧问题会让整份文件存不了。
pub fn check(kind: RawKind, old: &str, new: &str) -> RawCheck {
    let mut result = RawCheck {
        diff: unified_diff(old, new, file_label(kind)),
        ..RawCheck::default()
    };
    let value = match serde_yaml::from_str::<Value>(new) {
        Ok(value) => value,
        Err(e) => {
            result
                .errors
                .push(parse_issue(summon_i18n::text!("re12be01657"), &e));
            return result;
        }
    };
    let typed = match kind {
        RawKind::Config => serde_yaml::from_str::<Config>(new).map(|_| ()),
        RawKind::Agent => serde_yaml::from_str::<Agent>(new).map(|_| ()),
    };
    match typed {
        Err(e) => result
            .errors
            .push(parse_issue(summon_i18n::text!("rbab96212a8"), &e)),
        Ok(()) => {
            let old_value = serde_yaml::from_str::<Value>(old).ok();
            for issue in semantic_issues(kind, new) {
                let changed = issue.depends_on.iter().any(|dep| {
                    let path = split(dep);
                    old_value.as_ref().and_then(|v| value_at(v, &path)) != value_at(&value, &path)
                });
                let mut raw = RawIssue {
                    line: crate::yaml_edit::key_line(new, &split(issue.key)),
                    key: Some(issue.key.to_string()),
                    message: issue.message,
                    ..RawIssue::default()
                };
                if issue.hard && changed {
                    result.errors.push(raw);
                } else {
                    if issue.hard {
                        raw.message.push_str(summon_i18n::text!("rb0a07e2328"));
                    }
                    result.warnings.push(raw);
                }
            }
        }
    }
    key_issues(kind, new, &value, &mut result.warnings);
    result
}

/// 键路径在这份文件的结构里是否存在(字段表与覆盖测试用)。映射下面的任意键都算存在。
pub fn known_path(kind: RawKind, dotted: &str) -> bool {
    let root = shape(kind);
    let mut cur = &root;
    for seg in dotted.split('.') {
        cur = match cur {
            Shape::Struct { fields, .. } if fields.contains(&seg) => cur.child(seg),
            Shape::Map(inner) => inner,
            _ => return false,
        };
    }
    true
}

fn file_label(kind: RawKind) -> &'static str {
    match kind {
        RawKind::Config => "config.yaml",
        RawKind::Agent => "agent.yaml",
    }
}

fn split(dotted: &str) -> Vec<String> {
    dotted.split('.').map(str::to_string).collect()
}

fn value_at<'v>(root: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter()
        .try_fold(root, |cur, seg| cur.as_mapping()?.get(seg.as_str()))
}

fn parse_issue(prefix: &str, e: &serde_yaml::Error) -> RawIssue {
    let location = e.location();
    RawIssue {
        line: location.as_ref().map(|l| l.line()),
        col: location.as_ref().map(|l| l.column()),
        message: format!("{prefix}：{e}"),
        ..RawIssue::default()
    }
}

// ── 语义校验 ──────────────────────────────────────────────────────────────

struct Semantic {
    key: &'static str,
    message: String,
    /// 能解析但值不对:这次改到了就是错误。false = 只提示(危险组合、认不出的图标)。
    hard: bool,
    /// 这些键在这次改动里变了,才算「这次改出来的」。
    depends_on: &'static [&'static str],
}

fn semantic_issues(kind: RawKind, text: &str) -> Vec<Semantic> {
    let mut out = Vec::new();
    let mut push = |key: &'static str, depends_on: &'static [&'static str], message: String| {
        out.push(Semantic {
            key,
            message,
            hard: !depends_on.is_empty(),
            depends_on,
        })
    };
    let one_of = |values: &[&str]| values.join(" / ");
    let illegal = |v: Option<&str>, legal: &[&str]| -> Option<String> {
        v.filter(|v| !legal.contains(&v.trim())).map(str::to_string)
    };
    match kind {
        RawKind::Config => {
            let Ok(cfg) = serde_yaml::from_str::<Config>(text) else {
                return out;
            };
            if let Some(v) = illegal(cfg.codex.sandbox.as_deref(), SANDBOX_VALUES) {
                push(
                    "codex.sandbox",
                    &["codex.sandbox"],
                    summon_i18n::format!("r5677fe0a7f", one_of(SANDBOX_VALUES)),
                );
            }
            if let Some(v) = illegal(cfg.codex.approval_policy.as_deref(), APPROVAL_VALUES) {
                push(
                    "codex.approval_policy",
                    &["codex.approval_policy"],
                    summon_i18n::format!("r70f2841a0f", one_of(APPROVAL_VALUES)),
                );
            }
            let (policy, _) = codex_policy::resolve(Some(&cfg.codex), None, None);
            if let Some(warning) = policy.write_blocked_warning() {
                push("codex", &[], warning);
            }
            let icon = cfg.menubar.icon.as_deref();
            if let Some(icon) = icon.filter(|i| !MENUBAR_ICONS.iter().any(|m| m.name == i.trim())) {
                push("menubar.icon", &[], summon_i18n::format!("r3153f906fa"));
            }
        }
        RawKind::Agent => {
            let Ok(agent) = serde_yaml::from_str::<Agent>(text) else {
                return out;
            };
            let cli = agent.cli_kind();
            if CliDriver::from_kind(cli) == CliDriver::Other {
                push(
                    "runtime.cli",
                    &["runtime.cli"],
                    summon_i18n::format!("r731c3b5f8b"),
                );
            } else if let Err(e) = model_effort::validate(cli, agent.model(), agent.effort()) {
                // 换了 CLI 或模型也会让原来的档位不合法:三者任一变了都算这次改出来的。
                let key = if model_effort::validate(cli, agent.model(), None).is_err() {
                    "model"
                } else {
                    "effort"
                };
                push(
                    key,
                    &["model", "effort", "runtime.cli"],
                    summon_i18n::format!("r1ea8a163c0"),
                );
            }
            let runtime = agent.runtime.as_ref();
            if let Some(capture) = runtime.and_then(|r| r.capture.as_deref()) {
                if let Err(e) = CaptureOverride::parse(capture) {
                    push("runtime.capture", &["runtime.capture"], e.to_string());
                }
            }
            let sandbox = runtime.and_then(|r| r.codex_sandbox.as_deref());
            if let Some(v) = illegal(sandbox, SANDBOX_VALUES) {
                push(
                    "runtime.codex_sandbox",
                    &["runtime.codex_sandbox"],
                    summon_i18n::format!("r5677fe0a7f", one_of(SANDBOX_VALUES)),
                );
            }
            let approval = runtime.and_then(|r| r.codex_approval_policy.as_deref());
            if let Some(v) = illegal(approval, APPROVAL_VALUES) {
                push(
                    "runtime.codex_approval_policy",
                    &["runtime.codex_approval_policy"],
                    summon_i18n::format!("r70f2841a0f", one_of(APPROVAL_VALUES)),
                );
            }
            if let Some(v) = agent.reasoning_summary() {
                if reasoning_summary_decision(Some(v), true).as_deref() != Some(v) {
                    push(
                        "reasoning_summary",
                        &["reasoning_summary"],
                        summon_i18n::format!("rf0fead3a3a"),
                    );
                }
            }
        }
    }
    out
}

// ── 未知键与废弃键 ────────────────────────────────────────────────────────

/// 文件结构:结构体的字段清单(从 serde 派生里截下来),以及哪些字段下面还要往里看。
enum Shape {
    Struct {
        fields: &'static [&'static str],
        children: Vec<(&'static str, Shape)>,
    },
    /// 任意键的映射(chat id、环境变量名……),值的结构是 `.0`。
    Map(Box<Shape>),
    /// 列表,元素的结构是 `.0`。
    Seq(Box<Shape>),
    /// 不再往里看:标量,或交给 serde 自己严格校验的子结构。
    Leaf,
}

static LEAF: Shape = Shape::Leaf;

impl Shape {
    fn child(&self, key: &str) -> &Shape {
        match self {
            Shape::Struct { children, .. } => children
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, s)| s)
                .unwrap_or(&LEAF),
            Shape::Map(inner) => inner,
            _ => &LEAF,
        }
    }
}

fn map_of(inner: Shape) -> Shape {
    Shape::Map(Box::new(inner))
}

fn strukt<T: DeserializeOwned>(children: Vec<(&'static str, Shape)>) -> Shape {
    Shape::Struct {
        fields: fields_of::<T>(),
        children,
    }
}

fn messaging_shape() -> Shape {
    strukt::<MessagingOverride>(vec![
        ("session_exited", strukt::<SessionExitedOverride>(vec![])),
        ("slash_blacklist", strukt::<SlashBlacklistOverride>(vec![])),
        ("reaction_meanings", map_of(Shape::Leaf)),
        ("models_by_cli", map_of(Shape::Leaf)),
        ("default_model_by_cli", map_of(Shape::Leaf)),
    ])
}

fn shape(kind: RawKind) -> Shape {
    match kind {
        RawKind::Config => strukt::<Config>(vec![
            ("feishu", strukt::<GlobalFeishu>(vec![])),
            ("tools", strukt::<Tools>(vec![])),
            ("messaging", messaging_shape()),
            ("menubar", strukt::<MenuBar>(vec![])),
            (
                "sentinel",
                strukt::<Sentinel>(vec![("aliases", map_of(Shape::Leaf))]),
            ),
            ("remotes", Shape::Seq(Box::new(strukt::<Remote>(vec![])))),
            // 严格子结构:写错的键 serde 自己报错,这里不重复。
            ("execution_targets", map_of(Shape::Leaf)),
            ("claude", strukt::<Claude>(vec![])),
            ("codex", strukt::<CodexPolicyOverride>(vec![])),
            (
                "cursor",
                strukt::<Cursor>(vec![("quota", strukt::<CursorQuota>(vec![]))]),
            ),
            ("preview_server", strukt::<PreviewServer>(vec![])),
            ("web_terminal", strukt::<WebTerminal>(vec![])),
            ("dashboard", strukt::<Dashboard>(vec![])),
            ("upgrade", strukt::<Upgrade>(vec![])),
        ]),
        RawKind::Agent => strukt::<Agent>(vec![
            (
                "feishu",
                strukt::<Feishu>(vec![
                    ("chat_operators", map_of(Shape::Leaf)),
                    ("chat_reply_without_mention", map_of(Shape::Leaf)),
                ]),
            ),
            (
                "runtime",
                strukt::<Runtime>(vec![
                    ("env", map_of(Shape::Leaf)),
                    ("chat_working_dirs", map_of(Shape::Leaf)),
                ]),
            ),
            ("messaging", messaging_shape()),
            ("aliases", map_of(Shape::Leaf)),
        ]),
    }
}

/// serde 派生的结构体字段清单。派生出来的 `Deserialize` 一定先调 `deserialize_struct` 并把
/// 字段名交出来,这里截下它就返回;非结构体类型拿到空清单。
fn fields_of<T: DeserializeOwned>() -> &'static [&'static str] {
    struct Grab<'a>(&'a mut Option<&'static [&'static str]>);

    impl<'de> serde::Deserializer<'de> for Grab<'_> {
        type Error = serde::de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom(summon_i18n::text!("r3efb7dcc22")))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = Some(fields);
            Err(serde::de::Error::custom(summon_i18n::text!("r996ae966fc")))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
            identifier ignored_any
        }
    }

    let mut fields = None;
    // 截到清单之后故意返回错误;反序列化的结果本身没有用。
    let _ = T::deserialize(Grab(&mut fields));
    fields.unwrap_or(&[])
}

/// 结构里每一个叶子键的路径(子结构体展开,映射 / 列表 / 标量各算一个叶子)。
fn leaf_paths(kind: RawKind) -> Vec<String> {
    fn walk(shape: &Shape, prefix: &str, out: &mut Vec<String>) {
        let Shape::Struct { fields, .. } = shape else {
            return;
        };
        for field in *fields {
            let path = if prefix.is_empty() {
                field.to_string()
            } else {
                format!("{prefix}.{field}")
            };
            match shape.child(field) {
                child @ Shape::Struct { .. } => walk(child, &path, out),
                _ => out.push(path),
            }
        }
    }
    let mut out = Vec::new();
    walk(&shape(kind), "", &mut out);
    out
}

/// 这份文件里登记过的废弃键:`CONFIG_FIELD_META` 里标了废弃的(agent.yaml 只认 `messaging.*`
/// 那几条),加上字段表里登记的。返回 `(路径, 说明)`。
fn deprecated_entries(kind: RawKind) -> impl Iterator<Item = (&'static str, String)> {
    let meta = CONFIG_FIELD_META.iter().filter_map(move |m| {
        let applies = kind == RawKind::Config || m.path.starts_with("messaging.");
        let since = crate::i18n::label(m.deprecated_since.filter(|_| applies)?);
        Some((
            m.path,
            summon_i18n::format!("rcf180ea904", crate::i18n::label(m.description)),
        ))
    });
    let noted = KEY_NOTES
        .iter()
        .filter(move |n| n.file == kind && n.kind == NoteKind::Deprecated)
        .map(|n| {
            (
                n.path,
                summon_i18n::format!("r1396ec01f5", crate::i18n::label(n.note)),
            )
        });
    meta.chain(noted)
}

fn collect_unknown(
    value: &Value,
    shape: &Shape,
    path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    match (shape, value) {
        (Shape::Struct { fields, .. }, Value::Mapping(entries)) => {
            for (key, child) in entries {
                let Some(key) = key.as_str() else { continue };
                path.push(key.to_string());
                if fields.contains(&key) {
                    collect_unknown(child, shape.child(key), path, out);
                } else {
                    out.push(path.clone());
                }
                path.pop();
            }
        }
        (Shape::Map(inner), Value::Mapping(entries)) => {
            for (key, child) in entries {
                let Some(key) = key.as_str() else { continue };
                path.push(key.to_string());
                collect_unknown(child, inner, path, out);
                path.pop();
            }
        }
        (Shape::Seq(inner), Value::Sequence(items)) => {
            for item in items {
                collect_unknown(item, inner, path, out);
            }
        }
        _ => {}
    }
}

fn key_issues(kind: RawKind, text: &str, value: &Value, warnings: &mut Vec<RawIssue>) {
    let deprecated: Vec<(&str, String)> = deprecated_entries(kind).collect();
    for (path, message) in &deprecated {
        if path_exists_in_value(value, path) {
            warnings.push(RawIssue {
                line: crate::yaml_edit::key_line(text, &split(path)),
                key: Some(path.to_string()),
                message: format!("`{path}` {message}"),
                ..RawIssue::default()
            });
        }
    }
    let root = shape(kind);
    let mut unknown = Vec::new();
    collect_unknown(value, &root, &mut Vec::new(), &mut unknown);
    for path in unknown {
        let dotted = path.join(".");
        let quiet = key_note(kind, &dotted)
            .is_some_and(|n| matches!(n.kind, NoteKind::External | NoteKind::Deprecated));
        let covered = deprecated.iter().any(|(p, _)| {
            dotted == *p || dotted.strip_prefix(p).is_some_and(|r| r.starts_with('.'))
        });
        if quiet || covered {
            continue;
        }
        warnings.push(RawIssue {
            line: crate::yaml_edit::key_line(text, &path),
            key: Some(dotted.clone()),
            message: summon_i18n::format!("rd43f21f318"),
            suggestion: suggest(kind, &root, &path),
            ..RawIssue::default()
        });
    }
}

/// 最接近的合法键:别处恰好有同名的键(写错了层级)优先,其次是同一层里拼写最接近的。
fn suggest(kind: RawKind, root: &Shape, path: &[String]) -> Option<String> {
    let (key, parents) = path.split_last()?;
    // 写错了层级(顶层的 `cot_visible`),或少了前缀(agent.yaml 顶层的 `approval_policy`
    // 其实是 `runtime.codex_approval_policy`)。
    let suffix = format!("_{key}");
    let leaves = leaf_paths(kind);
    let last = |p: &String| p.rsplit('.').next().unwrap_or_default().to_string();
    let exact = leaves.iter().find(|p| last(p) == *key);
    if let Some(found) = exact.or_else(|| leaves.iter().find(|p| last(p).ends_with(&suffix))) {
        return Some(found.clone());
    }
    let mut parent = root;
    for seg in parents {
        parent = parent.child(seg);
    }
    let Shape::Struct { fields, .. } = parent else {
        return None;
    };
    let limit = (key.chars().count() / 3).max(1);
    let (best, distance) = fields
        .iter()
        .map(|f| (*f, levenshtein(key, f)))
        .min_by_key(|(_, d)| *d)?;
    (distance <= limit).then(|| {
        let mut full: Vec<&str> = parents.iter().map(String::as_str).collect();
        full.push(best);
        full.join(".")
    })
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(ca != *cb);
            cur[j + 1] = substitute.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

// ── 差异 ──────────────────────────────────────────────────────────────────

const CONTEXT: usize = 3;
/// 逐行 LCS 的表格上限;超过就整段记为「删掉旧的、换上新的」,不为一次预览吃掉几百 MB。
const LCS_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Same,
    Del,
    Ins,
}

/// 统一差异格式(3 行上下文)。两边相同时为空串。
pub fn unified_diff(old: &str, new: &str, label: &str) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = format!("--- a/{label}\n+++ b/{label}\n");
    if a == b {
        out.push_str(summon_i18n::text!("r2013a96bfc"));
        return out;
    }
    // (操作, 旧文件里此前走过的行数, 新文件里此前走过的行数)
    let mut steps = Vec::new();
    let (mut ai, mut bi) = (0, 0);
    for op in diff_ops(&a, &b) {
        steps.push((op, ai, bi));
        match op {
            Op::Same => {
                ai += 1;
                bi += 1;
            }
            Op::Del => ai += 1,
            Op::Ins => bi += 1,
        }
    }
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for (k, _) in steps.iter().enumerate().filter(|(_, s)| s.0 != Op::Same) {
        let start = k.saturating_sub(CONTEXT);
        let end = (k + CONTEXT).min(steps.len() - 1);
        match hunks.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => hunks.push((start, end)),
        }
    }
    let range = |before: usize, count: usize| match count {
        0 => format!("{before},0"),
        1 => format!("{}", before + 1),
        _ => format!("{},{count}", before + 1),
    };
    for (start, end) in hunks {
        let span = &steps[start..=end];
        let old_count = span.iter().filter(|s| s.0 != Op::Ins).count();
        let new_count = span.iter().filter(|s| s.0 != Op::Del).count();
        let (_, a0, b0) = span[0];
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(a0, old_count),
            range(b0, new_count)
        ));
        for &(op, ai, bi) in span {
            let (mark, line) = match op {
                Op::Same => (' ', a[ai]),
                Op::Del => ('-', a[ai]),
                Op::Ins => ('+', b[bi]),
            };
            out.push(mark);
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops = vec![Op::Same; prefix];
    let (n, m) = (am.len(), bm.len());
    if n.saturating_mul(m) > LCS_CELLS {
        ops.extend(std::iter::repeat_n(Op::Del, n));
        ops.extend(std::iter::repeat_n(Op::Ins, m));
    } else {
        // lcs[i * width + j] = am[i..] 与 bm[j..] 的最长公共子序列长度。
        let width = m + 1;
        let mut lcs = vec![0u32; (n + 1) * width];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * width + j] = if am[i] == bm[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(Op::Same);
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[(i + 1) * width + j] >= lcs[i * width + j + 1]) {
                ops.push(Op::Del);
                i += 1;
            } else {
                ops.push(Op::Ins);
                j += 1;
            }
        }
    }
    ops.extend(std::iter::repeat_n(Op::Same, suffix));
    ops
}

#[cfg(test)]
mod tests {
    use super::super::fields::FIELDS;
    use super::*;

    const AGENT: &str = "name: a\nfeishu:\n  app_id_env: A_ID\n  allowed_users: [on_me]\n\
                         runtime:\n  cli: claude\n";

    fn warning_keys(check: &RawCheck) -> Vec<String> {
        check
            .warnings
            .iter()
            .filter_map(|w| w.key.clone())
            .collect()
    }

    #[test]
    fn strict_substructure_unknown_key_is_error() {
        let cfg = "execution_targets:\n  box:\n    type: ssh\n    host: h\n    bogus: 1\n";
        let result = check(RawKind::Config, "", cfg);
        assert!(
            !result.ok(),
            "严格子结构里的未知键让整份文件读不出,必须挡下"
        );
        assert!(result.errors[0].line.is_some(), "{:?}", result.errors);
        let agent = format!("{AGENT}  execution:\n    target: box\n    bogus: 1\n");
        let result = check(RawKind::Agent, AGENT, &agent);
        assert!(!result.ok(), "{result:?}");
    }

    #[test]
    fn external_reader_keys_do_not_warn() {
        let agent = format!("{AGENT}cognition:\n  history_lookback_days: 7\n  writeback: true\n");
        let result = check(RawKind::Agent, AGENT, &agent);
        assert!(result.ok() && result.warnings.is_empty(), "{result:?}");
        // 已下线的段:不是外部读取,是废弃。
        let cfg = "quota_recovery:\n  enabled: false\n";
        let result = check(RawKind::Config, "", cfg);
        assert!(result.ok(), "{result:?}");
        assert_eq!(warning_keys(&result), ["quota_recovery"]);
        assert!(result.warnings[0].message.contains("下线"), "{result:?}");
    }

    #[test]
    fn typo_warns_with_nearest_key() {
        let cfg = "messaging:\n  cot_visable: false\n";
        let result = check(RawKind::Config, "", cfg);
        assert!(
            result.ok(),
            "顶层结构体不拒未知键,文件照样能读,只是这一项被忽略"
        );
        let w = &result.warnings[0];
        assert_eq!(w.key.as_deref(), Some("messaging.cot_visable"));
        assert_eq!(w.suggestion.as_deref(), Some("messaging.cot_visible"));
        assert_eq!(w.line, Some(2));
        // 写错了层级:按最后一段找到别处的合法键。
        let result = check(RawKind::Config, "", "cot_visible: false\n");
        assert_eq!(
            result.warnings[0].suggestion.as_deref(),
            Some("messaging.cot_visible")
        );
        // agent.yaml 顶层的手误同样报出来。
        let agent = format!("{AGENT}approval_policy: never\n");
        let result = check(RawKind::Agent, AGENT, &agent);
        assert_eq!(warning_keys(&result), ["approval_policy"]);
        assert_eq!(
            result.warnings[0].suggestion.as_deref(),
            Some("runtime.codex_approval_policy")
        );
        // 离哪个都远:不硬给建议。
        let result = check(RawKind::Config, "", "zzz_qqq: 1\n");
        assert_eq!(result.warnings[0].suggestion, None);
    }

    #[test]
    fn deprecated_key_warns() {
        let cfg =
            "tmux_bin: /usr/bin/tmux\ngateway:\n  port: 1\npreview_server:\n  token_ttl_days: 3\n";
        let result = check(RawKind::Config, "", cfg);
        assert!(result.ok(), "{result:?}");
        let mut keys = warning_keys(&result);
        keys.sort();
        assert_eq!(
            keys,
            ["gateway", "preview_server.token_ttl_days", "tmux_bin"]
        );
        let agent = format!("{AGENT}messaging:\n  interrupt_enabled: true\n");
        let result = check(RawKind::Agent, AGENT, &agent);
        assert_eq!(warning_keys(&result), ["messaging.interrupt_enabled"]);
    }

    #[test]
    fn parse_error_reports_line_and_col() {
        let result = check(
            RawKind::Config,
            "",
            "messaging:\n  cot_visible: [true\npoll_ms: 1\n",
        );
        assert!(!result.ok());
        assert!(result.errors[0].line.is_some() && result.errors[0].col.is_some());
        // 类型不对也是错误,并带行号。
        let result = check(RawKind::Config, "", "poll_ms: 快\n");
        assert_eq!(result.errors[0].line, Some(1), "{result:?}");
        // agent.yaml 少了必填的 name。
        assert!(!check(RawKind::Agent, AGENT, "display: x\n").ok());
    }

    #[test]
    fn semantic_values_fail_only_when_changed() {
        let bad = format!("{AGENT}effort: ultra-max\n");
        let result = check(RawKind::Agent, AGENT, &bad);
        assert_eq!(result.errors.len(), 1, "{result:?}");
        assert_eq!(result.errors[0].key.as_deref(), Some("effort"));
        // 原本就坏着、这次没动:降为警告,不挡与它无关的保存。
        let touched = format!("{bad}display: 新名字\n");
        let result = check(RawKind::Agent, &bad, &touched);
        assert!(result.ok(), "{result:?}");
        assert!(warning_keys(&result).contains(&"effort".to_string()));
        let cfg = "codex:\n  sandbox: read_only\n";
        let result = check(RawKind::Config, "", cfg);
        assert_eq!(result.errors[0].key.as_deref(), Some("codex.sandbox"));
        let capture = format!("{AGENT}  capture: exe\n");
        assert!(!check(RawKind::Agent, AGENT, &capture).ok());
        // 危险组合当场提示。
        let cfg = "codex:\n  sandbox: read-only\n  approval_policy: never\n";
        let result = check(RawKind::Config, "", cfg);
        assert!(result.ok());
        assert_eq!(warning_keys(&result), ["codex"]);
    }

    #[test]
    fn diff_is_unified_and_keeps_placeholders() {
        let old = "a: 1\nb: 2\nc: 3\nd: 4\ne: 5\nf: 6\ng: 7\nh: 8\nenv:\n  K: «summon:redacted»\n";
        let new = "a: 1\nb: 20\nc: 3\nd: 4\ne: 5\nf: 6\ng: 7\nh: 8\nenv:\n  K: «summon:redacted»\n  L: x\n";
        let diff = unified_diff(old, new, "agent.yaml");
        assert_eq!(
            diff,
            "--- a/agent.yaml\n+++ b/agent.yaml\n\
             @@ -1,5 +1,5 @@\n a: 1\n-b: 2\n+b: 20\n c: 3\n d: 4\n e: 5\n\
             @@ -8,3 +8,4 @@\n h: 8\n env:\n   K: «summon:redacted»\n+  L: x\n"
        );
        assert_eq!(unified_diff(old, old, "agent.yaml"), "");
        let result = check(RawKind::Config, "", "poll_ms: 1\n");
        assert!(result
            .diff
            .starts_with("--- a/config.yaml\n+++ b/config.yaml\n@@ -0,0 +1 @@\n"));
        assert!(unified_diff("a\n", "a\r\n", "x").contains("换行"));
    }

    #[test]
    fn every_struct_key_is_a_field_or_noted() {
        // 字段清单真的截到了:否则下面的覆盖检查会空转。
        let config = leaf_paths(RawKind::Config);
        assert!(config.len() > 100, "{}", config.len());
        assert!(config.contains(&"tools.tmux".to_string()));
        assert!(config.contains(&"cursor.quota.interval_secs".to_string()));
        assert!(leaf_paths(RawKind::Agent).contains(&"runtime.env".to_string()));
        for kind in [RawKind::Config, RawKind::Agent] {
            for path in leaf_paths(kind) {
                let as_field = FIELDS.iter().any(|f| {
                    f.in_file(kind)
                        && (f.key == path
                            || f.key
                                .strip_prefix(path.as_str())
                                .is_some_and(|r| r.starts_with('.')))
                });
                let deprecated = deprecated_entries(kind).any(|(p, _)| p == path);
                assert!(
                    as_field || key_note(kind, &path).is_some() || deprecated,
                    "{kind:?} 的 {path} 既不在字段表里,也没登记为原文模式 / 专用页面 / 已废弃"
                );
            }
        }
    }

    #[test]
    fn field_keys_exist_in_their_files() {
        for f in FIELDS {
            for kind in [RawKind::Config, RawKind::Agent] {
                if f.in_file(kind) {
                    assert!(known_path(kind, f.key), "{kind:?} 里没有 {}", f.key);
                }
            }
        }
        assert!(!known_path(RawKind::Config, "messaging.nope"));
        assert!(known_path(RawKind::Agent, "runtime.env.ANY"));
    }
}
