//! 定时器纯件:Schedule 模型 + 补火决策 + 命令解析 + 卡片表单转换(无 IO/无网络,必单测)。
//! 权威 spec:docs/superpowers/specs/2026-06-14-p2-automation-timers-design.md
//! 卡片化:docs/spec/2026-08-03-schedule-card-ui-design.md

pub mod cron;
pub mod form;
pub mod next_fire;

use chrono::{DateTime, Duration, Local};
use serde::{Deserialize, Serialize};

/// 定时节律。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// 一次性:`spec` = RFC3339 时刻(本地时区)。
    Once,
    /// 周期:`spec` = 秒数。
    Interval,
    /// 周期:`spec` = 5 段 cron(分 时 日 月 周,本地时区)。
    Cron,
}

/// 定时轮向会话展示多少执行信息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleOutput {
    /// 展示流式回复、工具轨迹与最终结果。
    #[default]
    Progress,
    /// 执行期间不推送过程，只交付本轮最终结果。
    Result,
}

impl ScheduleOutput {
    pub fn label(self) -> &'static str {
        match self {
            Self::Progress => summon_i18n::text!("r1fd525f2e1"),
            Self::Result => summon_i18n::text!("r36b1b0b39f"),
        }
    }
}

impl std::str::FromStr for ScheduleOutput {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "progress" => Ok(Self::Progress),
            "result" => Ok(Self::Result),
            _ => Err(summon_i18n::format!("rbb88f9d96a")),
        }
    }
}

/// 定时轮起跑时沿用哪份上下文。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleContext {
    /// 续用该 chat 现有会话(resume 记录 / 常驻进程)。
    #[default]
    Resume,
    /// 起跑前清 resume 记录并拆掉旧会话,本轮从空上下文冷启。
    /// 给状态全在文件里的周期巡检用:跨天累积的会话历史对当轮没有信息增量,却让每轮都以
    /// 冷缓存重写全部历史。
    Fresh,
}

impl ScheduleContext {
    pub fn label(self) -> &'static str {
        match self {
            Self::Resume => summon_i18n::text!("rdec4f9bead"),
            Self::Fresh => summon_i18n::text!("r701c8a3e76"),
        }
    }
}

impl std::str::FromStr for ScheduleContext {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "resume" => Ok(Self::Resume),
            "fresh" => Ok(Self::Fresh),
            _ => Err(summon_i18n::format!("rf10885e935")),
        }
    }
}

/// 一条定时器(落 `~/.summon/schedules.yaml`,可变 → YAML)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub kind: Kind,
    pub spec: String,
    pub agent: String,
    pub chat_id: String,
    pub chat_type: String,
    pub prompt: String,
    pub created_by: String,
    pub created_at: String,
    /// 缺省保持完整过程，兼容已有 schedules.yaml。
    #[serde(default)]
    pub output: ScheduleOutput,
    /// 缺省续用会话,兼容已有 schedules.yaml。
    #[serde(default)]
    pub context: ScheduleContext,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 上次触发时刻(RFC3339);落盘防重复 + 跨重启补火判定。
    #[serde(default)]
    pub last_fired: Option<String>,
    /// 内容改动次数：节律、正文、输出方式、上下文模式改动时加一，停用 / 启用不加。触发编号带上它，
    /// 改动后的触发不会撞上改动前那次的收件记录。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revision: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn default_true() -> bool {
    true
}

/// `parse_when` 的结果:kind + 存盘 spec + 吃掉的 token 数(其余 token = prompt)。
#[derive(Debug)]
pub struct When {
    pub kind: Kind,
    pub spec: String,
    pub consumed: usize,
}

/// 合法节律的**形态**清单 —— 报错时原样附上。
///
/// 只列名字(`in|at|every|…`)是不够的:调用方多半是 agent,它拿到「用 daily」仍然不知道
/// 后面该跟 `09:00` 还是 `9` 还是别的。错误消息就是它的文档,所以给完整例子。
pub const RHYTHM_FORMS: &str = "r42fd288728";

/// 把命令首部 token 解析成 kind+spec。语法:
/// `in <dur>` / `at <rfc3339>` → once;`every <dur>` → interval(秒);`cron <5 段>` → cron。
/// `<dur>` = `<n>s|m|h|d`。失败返回友好错误串(命令侧渲染)。
pub fn parse_when(tokens: &[&str], now: DateTime<Local>) -> Result<When, String> {
    let head = tokens.first().ok_or_else(|| {
        summon_i18n::format!(
            "r447204f472",
            RHYTHM_FORMS = ::summon_core::i18n::label(RHYTHM_FORMS)
        )
    })?;
    match *head {
        "in" => {
            let dur = tokens
                .get(1)
                .ok_or_else(|| summon_i18n::text!("rf3d357040f").to_string())?;
            let secs = parse_dur_secs(dur)?;
            // `parse_dur_secs` 的上界已让这里不可能溢出;仍走 checked,是为了让「本函数不 panic」
            // 这条性质在本地可证,而不依赖调用方保持某个不变量。
            let when = Duration::try_seconds(secs)
                .and_then(|d| now.checked_add_signed(d))
                .ok_or_else(|| summon_i18n::format!("rde7f42bb01"))?;
            Ok(When {
                kind: Kind::Once,
                spec: when.to_rfc3339(),
                consumed: 2,
            })
        }
        "at" => {
            let ts = tokens
                .get(1)
                .ok_or_else(|| summon_i18n::text!("r3114fc3c17").to_string())?;
            // 秒可省。`at 2026-06-15T15:00` 是人最自然的写法,原先只认到秒的格式,少写两位就整条
            // 报错——这个坑不该由用户来记。
            let naive = ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
                .iter()
                .find_map(|f| chrono::NaiveDateTime::parse_from_str(ts, f).ok());
            let when = DateTime::parse_from_rfc3339(ts)
                .map(|d| d.with_timezone(&Local))
                .map_err(|e| summon_i18n::format!("r578d12852a"))
                .or_else(|e| {
                    let n = naive.ok_or(e)?;
                    use chrono::TimeZone;
                    Local
                        .from_local_datetime(&n)
                        .earliest()
                        .ok_or_else(|| summon_i18n::text!("r9df6824c9e").to_string())
                })?;
            Ok(When {
                kind: Kind::Once,
                spec: when.to_rfc3339(),
                consumed: 2,
            })
        }
        "every" => {
            let dur = tokens
                .get(1)
                .ok_or_else(|| summon_i18n::text!("rc474030225").to_string())?;
            let secs = parse_dur_secs(dur)?;
            if secs < 60 {
                return Err(summon_i18n::text!("r38517e0c2b").to_string());
            }
            Ok(When {
                kind: Kind::Interval,
                spec: secs.to_string(),
                consumed: 2,
            })
        }
        "cron" => {
            if tokens.len() < 6 {
                return Err(summon_i18n::text!("re6dd068a5d").to_string());
            }
            let spec = tokens[1..6].join(" ");
            cron::parse_cron(&spec)?;
            Ok(When {
                kind: Kind::Cron,
                spec,
                consumed: 6,
            })
        }
        // 以下三种是卡片上就有的节律(每天 / 工作日 / 每周),落盘同样是 cron。
        // 用 `cron_spec` 与卡片走同一条拼装 —— 两个入口生成的 spec 必须逐字节一致,
        // 否则 `/schedules` 上同一个意思会显示成两种样子。
        "daily" => {
            let (h, m) = clock(tokens.get(1), "daily")?;
            Ok(When {
                kind: Kind::Cron,
                spec: cron_spec(m, h, "*"),
                consumed: 2,
            })
        }
        "weekday" => {
            let (h, m) = clock(tokens.get(1), "weekday")?;
            Ok(When {
                kind: Kind::Cron,
                spec: cron_spec(m, h, "1-5"),
                consumed: 2,
            })
        }
        "weekly" => {
            let days = tokens
                .get(1)
                .ok_or_else(|| summon_i18n::text!("r1fa22d96f9").to_string())?;
            let dow = parse_dow_list(days)?;
            let (h, m) = clock(tokens.get(2), "weekly mon")?;
            Ok(When {
                kind: Kind::Cron,
                spec: cron_spec(m, h, &cron::dow_field(&dow)),
                consumed: 3,
            })
        }
        other => Err(summon_i18n::format!(
            "r6af61dc3fc",
            ::summon_core::i18n::label(RHYTHM_FORMS)
        )),
    }
}

/// 拼 5 段 cron 并当场自校验 —— 拼错了要在这里炸,而不是落盘后由 `due_plan` 静默永不触发。
fn cron_spec(minute: u32, hour: u32, dow: &str) -> String {
    let spec = format!("{minute} {hour} * * {dow}");
    debug_assert!(
        cron::parse_cron(&spec).is_ok(),
        "{}",
        summon_i18n::format!("r17c0c304fb")
    );
    spec
}

fn clock(token: Option<&&str>, usage: &str) -> Result<(u32, u32), String> {
    let raw = token.ok_or_else(|| summon_i18n::format!("rfa4fac2d49"))?;
    form::hhmm(raw)
}

/// `mon` / `mon,wed` / `1,3` / `周一` → cron 的 dow 数字列表(0=周日,与 cron 惯例一致)。
///
/// 去重并排序:`mon,mon` 落成 `1,1` 会让 `/schedules` 上的人话渲染出「每周一、周一」。
fn parse_dow_list(raw: &str) -> Result<Vec<u32>, String> {
    let mut out: Vec<u32> = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        out.push(parse_dow(part)?);
    }
    if out.is_empty() {
        return Err(summon_i18n::text!("reb58dfbd5a").to_string());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn parse_dow(raw: &str) -> Result<u32, String> {
    let lower = raw.to_ascii_lowercase();
    // 只认前三个字母:`mon` / `monday` / `Mon.` 都该是周一,而不是让人去记哪种写法被接受。
    let head: String = lower
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .take(3)
        .collect();
    let by_name = match head.as_str() {
        "sun" => Some(0),
        "mon" => Some(1),
        "tue" => Some(2),
        "wed" => Some(3),
        "thu" => Some(4),
        "fri" => Some(5),
        "sat" => Some(6),
        _ => None,
    };
    if let Some(n) = by_name {
        return Ok(n);
    }
    match raw.trim() {
        summon_i18n::source!("ree239f3943")
        | summon_i18n::source!("rf9955c9dcc")
        | summon_i18n::source!("r7e06f3148f")
        | summon_i18n::source!("rec93da23f2")
        | "0"
        | "7" => Ok(0),
        summon_i18n::source!("rc430f4c12e") | summon_i18n::source!("r56451fdf19") | "1" => Ok(1),
        summon_i18n::source!("rd1378a68e6") | summon_i18n::source!("r8621c00d3b") | "2" => Ok(2),
        summon_i18n::source!("r2961168962") | summon_i18n::source!("r23ae0a11ac") | "3" => Ok(3),
        summon_i18n::source!("re9d3b01a5a") | summon_i18n::source!("r478911d29a") | "4" => Ok(4),
        summon_i18n::source!("r572388f3b0") | summon_i18n::source!("r6e5969e50e") | "5" => Ok(5),
        summon_i18n::source!("rf9aa11dbb1") | summon_i18n::source!("r7bb2f5ecaa") | "6" => Ok(6),
        other => Err(summon_i18n::format!("reb833fb939")),
    }
}

/// 时长业务上界:10 年(3650d = 315,360,000s)。
/// why 要有上界:① `/remind` 语义是提醒,超过 10 年的定时器没有现实用途;② 把秒数钉在安全区后,
/// `now + Duration::seconds(secs)` 这类日期算术在整个链路上都不可能溢出(`DateTime<Local>` 上界
/// 约公元 262143 年,chrono 的 `Add` 内部是 `expect`,溢出即 panic,release 也一样)。
/// 写入侧卡死上界 = 治本;读盘侧只做静默让路。
const MAX_DUR_SECS: i64 = 10 * 365 * 86_400;

fn parse_dur_secs(s: &str) -> Result<i64, String> {
    // 按「最后一个字符」切,不是按最后一个字节——`split_at` 落在多字节字符中间会 panic
    // (`/remind in 30秒` 就能踩到)。
    let unit = s
        .chars()
        .next_back()
        .ok_or_else(|| summon_i18n::format!("r7edf39a9ba"))?;
    let num = &s[..s.len() - unit.len_utf8()];
    let n: i64 = num
        .parse()
        .map_err(|_| summon_i18n::format!("r7edf39a9ba"))?;
    let mult: i64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return Err(summon_i18n::format!("r906cef1c16")),
    };
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| summon_i18n::format!("r0702391b02"))?;
    // 用区间判定而不是 `secs.abs()`:`i64::MIN.abs()` 自身就会溢出 panic。
    if !(-MAX_DUR_SECS..=MAX_DUR_SECS).contains(&secs) {
        return Err(summon_i18n::format!("r0702391b02"));
    }
    Ok(secs)
}

/// 一次 tick 对某 schedule 的决策。
#[derive(Debug, Clone, PartialEq)]
pub struct FirePlan {
    pub fire_now: bool,
    pub next_fire: Option<DateTime<Local>>,
    pub disable: bool,
}

/// 给定 now,判定是否到期、下次何时、是否放完即停。**去重靠 last_fired**:只有「应触发槽」严格晚于
/// last_fired 且 <= now 才 fire;错过多个周期只补一次(不雪崩)。disabled 恒不 fire。
pub fn due_plan(s: &Schedule, now: DateTime<Local>) -> FirePlan {
    let none = FirePlan {
        fire_now: false,
        next_fire: None,
        disable: false,
    };
    if !s.enabled {
        return none;
    }
    let last = s.last_fired.as_deref().and_then(parse_rfc3339);
    let created = parse_rfc3339_loose(&s.created_at).unwrap_or(now);
    match s.kind {
        Kind::Once => {
            let when = match parse_rfc3339(&s.spec) {
                Some(w) => w,
                None => return none,
            };
            if last.is_none() && now >= when {
                FirePlan {
                    fire_now: true,
                    next_fire: None,
                    disable: true,
                }
            } else {
                none
            }
        }
        Kind::Interval => {
            let secs: i64 = s.spec.parse().unwrap_or(0);
            if secs <= 0 {
                return none;
            }
            // 老版本没有写入上界,盘上可能已有毒 spec(如 i64::MAX)。读盘路径遇到坏数据静默让路
            // ——既不 panic 掉 scheduler 线程(单线程承载全部 agent 的定时器),也不 disable
            // (改盘是写入侧的事,读侧不该替用户做破坏性决定)。
            let step = match Duration::try_seconds(secs) {
                Some(d) => d,
                None => return none,
            };
            let base = last.unwrap_or(created);
            match base.checked_add_signed(step) {
                Some(due) if now >= due => FirePlan {
                    fire_now: true,
                    next_fire: now.checked_add_signed(step),
                    disable: false,
                },
                _ => none,
            }
        }
        Kind::Cron => {
            let e = match cron::parse_cron(&s.spec) {
                Ok(e) => e,
                Err(_) => return none,
            };
            let from = last.unwrap_or(created);
            let slot = cron::next_after(&e, from);
            if slot <= now {
                FirePlan {
                    fire_now: true,
                    next_fire: Some(cron::next_after(&e, now)),
                    disable: false,
                }
            } else {
                none
            }
        }
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Local))
}

/// 一条定时器的人话描述。**列表卡 / 创建与编辑回执 / CLI `schedules ls` 共用这一个函数**——
/// 三处若各写各的,同一条定时器在不同地方会显示成不同的东西,而这正是用户判断「我排对了没」的依据。
///
/// 说不清的一律**原样回显原始 spec**(见 `cron::human`),宁可给出用户看不懂但准确的表达式,
/// 也不给似是而非的中文。
pub fn human_readable(s: &Schedule) -> String {
    match s.kind {
        Kind::Once => match parse_rfc3339(&s.spec) {
            Some(t) => summon_i18n::format!("ra7610095c6", t.format("%Y-%m-%d %H:%M")),
            None => summon_i18n::format!("rba353b1180", s.spec),
        },
        Kind::Interval => match s.spec.parse::<i64>() {
            Ok(secs) if secs > 0 => summon_i18n::format!("rb75ee8b43c", human_secs(secs)),
            _ => summon_i18n::format!("rc2ab64bf6e", s.spec),
        },
        Kind::Cron => cron::human(&s.spec).unwrap_or_else(|| format!("cron `{}`", s.spec)),
    }
}

/// 从定时触发的 fire_id(`sched-<定时器 id>-<代际>`,修订号非 0 时再带 `-r<修订号>`)取回
/// 定时器 id,群里说「是哪一条」用它。认不出格式就原样返回 fire_id —— 那串至少还能在日志里对上。
pub fn schedule_id_of_fire(fire_id: &str) -> &str {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let Some(rest) = fire_id.strip_prefix("sched-") else {
        return fire_id;
    };
    let rest = match rest.rsplit_once('-') {
        Some((head, last)) if last.strip_prefix('r').is_some_and(digits) => head,
        _ => rest,
    };
    match rest.rsplit_once('-') {
        Some((id, generation)) if !id.is_empty() && digits(generation) => id,
        _ => fire_id,
    }
}

/// 定时轮开跑后、交付前被打断时发给该群的通知。
pub fn interrupted_fire_notice(fire_id: &str) -> String {
    summon_i18n::format!("rf423a747c4", schedule_id_of_fire(fire_id))
}

/// 秒 → 人话时长。只在整除时用大单位,`5400` 保持「90 分钟」而不是「1.5 小时」。
fn human_secs(secs: i64) -> String {
    match secs {
        s if s % 86_400 == 0 => summon_i18n::format!("r758b17ae26", s / 86_400),
        s if s % 3_600 == 0 => summon_i18n::format!("r8ba62c6229", s / 3_600),
        s if s % 60 == 0 => summon_i18n::format!("ra8442c80e9", s / 60),
        s => summon_i18n::format!("r91a4473b5c"),
    }
}

/// created_at 历史是 `%Y-%m-%d %H:%M:%S` 本地时区(与 .meta 同);兼容 RFC3339。
fn parse_rfc3339_loose(s: &str) -> Option<DateTime<Local>> {
    parse_rfc3339(s).or_else(|| {
        use chrono::TimeZone;
        chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S")
            .ok()
            .and_then(|n| Local.from_local_datetime(&n).earliest())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_id_of_fire_strips_prefix_and_generation() {
        assert_eq!(schedule_id_of_fire("sched-SCH-1-0"), "SCH-1");
        assert_eq!(schedule_id_of_fire("sched-SCH-7-1792118280000"), "SCH-7");
        assert_eq!(schedule_id_of_fire("sched-SCH-153-0-r1"), "SCH-153");
        assert_eq!(
            schedule_id_of_fire("sched-SCH-7-1792118280000-r12"),
            "SCH-7"
        );
        for odd in [
            "om_x100b",
            "sched-",
            "sched--0",
            "sched-SCH",
            "sched-SCH-r1",
        ] {
            assert_eq!(schedule_id_of_fire(odd), odd, "认不出就原样返回");
        }
        assert_eq!(
            interrupted_fire_notice("sched-SCH-1-0"),
            "⏰ 定时任务 SCH-1 被打断，未送达。"
        );
        assert_eq!(
            interrupted_fire_notice("sched-SCH-153-0-r2"),
            "⏰ 定时任务 SCH-153 被打断，未送达。"
        );
    }

    #[test]
    fn schedule_output_defaults_to_progress_and_round_trips_result() {
        let legacy = "id: SCH-1\nkind: cron\nspec: \"0 9 * * *\"\nagent: a\n\
                      chat_id: c\nchat_type: group\nprompt: p\ncreated_by: ou\n\
                      created_at: \"2026-08-24T09:00:00+08:00\"\n";
        let parsed: Schedule = serde_yaml::from_str(legacy).unwrap();
        assert_eq!(parsed.output, ScheduleOutput::Progress);

        let mut result_only = parsed;
        result_only.output = ScheduleOutput::Result;
        let yaml = serde_yaml::to_string(&result_only).unwrap();
        assert!(yaml.contains("output: result"));
        assert_eq!(
            serde_yaml::from_str::<Schedule>(&yaml).unwrap().output,
            ScheduleOutput::Result
        );
    }

    #[test]
    fn schedule_context_defaults_to_resume_and_round_trips_fresh() {
        let legacy = "id: SCH-1\nkind: cron\nspec: \"0 9 * * *\"\nagent: a\n\
                      chat_id: c\nchat_type: group\nprompt: p\ncreated_by: ou\n\
                      created_at: \"2026-08-24T09:00:00+08:00\"\n";
        let parsed: Schedule = serde_yaml::from_str(legacy).unwrap();
        assert_eq!(parsed.context, ScheduleContext::Resume);

        let mut fresh = parsed;
        fresh.context = ScheduleContext::Fresh;
        let yaml = serde_yaml::to_string(&fresh).unwrap();
        assert!(yaml.contains("context: fresh"));
        assert_eq!(
            serde_yaml::from_str::<Schedule>(&yaml).unwrap().context,
            ScheduleContext::Fresh
        );
        assert_eq!(
            "fresh".parse::<ScheduleContext>().unwrap(),
            ScheduleContext::Fresh
        );
        assert_eq!(
            "resume".parse::<ScheduleContext>().unwrap(),
            ScheduleContext::Resume
        );
        assert!("new".parse::<ScheduleContext>().is_err());
    }

    #[test]
    fn schedule_yaml_round_trips() {
        let y = "id: SCH-1\nkind: cron\nspec: \"0 9 * * 1-5\"\nagent: ambrose\n\
                 chat_id: oc_x\nchat_type: group\nprompt: 跑日报\ncreated_by: ou_x\n\
                 created_at: \"2026-06-14 12:00:00\"\nenabled: true\nlast_fired: null\n";
        let s: Schedule = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.id, "SCH-1");
        assert_eq!(s.kind, Kind::Cron);
        assert_eq!(s.spec, "0 9 * * 1-5");
        assert!(s.enabled);
        assert!(s.last_fired.is_none());
        let back = serde_yaml::to_string(&s).unwrap();
        let s2: Schedule = serde_yaml::from_str(&back).unwrap();
        assert_eq!(s2.prompt, "跑日报");
    }

    #[test]
    fn human_readable_covers_three_kinds() {
        assert_eq!(
            human_readable(&sched(Kind::Cron, "0 9 * * 1-5")),
            "每个工作日 09:00"
        );
        assert_eq!(
            human_readable(&sched(Kind::Interval, "7200")),
            "每隔 2 小时"
        );
        // 不整除小时就退回分钟,不编出「1.5 小时」。
        assert_eq!(
            human_readable(&sched(Kind::Interval, "5400")),
            "每隔 90 分钟"
        );
        // 一次性的时刻按本机时区写、按本机时区读,不随跑测试的机器变。
        use chrono::TimeZone;
        let once_at = chrono::Local
            .with_ymd_and_hms(2026, 8, 4, 9, 0, 0)
            .unwrap()
            .to_rfc3339();
        assert_eq!(
            human_readable(&sched(Kind::Once, &once_at)),
            "2026-08-04 09:00 一次"
        );
    }

    #[test]
    fn human_readable_falls_back_to_raw_spec() {
        // 盘上可能有手写的复杂 cron 或历史坏数据:一律原样回显,绝不猜、更不 panic。
        assert_eq!(
            human_readable(&sched(Kind::Cron, "*/15 9-18 * * *")),
            "cron `*/15 9-18 * * *`"
        );
        assert_eq!(human_readable(&sched(Kind::Cron, "坏了")), "cron `坏了`");
        assert_eq!(human_readable(&sched(Kind::Interval, "0")), "间隔 `0`");
        assert_eq!(human_readable(&sched(Kind::Interval, "x")), "间隔 `x`");
        assert_eq!(human_readable(&sched(Kind::Once, "x")), "一次 `x`");
    }

    #[test]
    fn kind_serde_is_lowercase() {
        assert_eq!(serde_yaml::to_string(&Kind::Once).unwrap().trim(), "once");
        assert_eq!(
            serde_yaml::to_string(&Kind::Interval).unwrap().trim(),
            "interval"
        );
    }

    #[test]
    fn parse_when_covers_in_at_every_cron() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        let w = parse_when(&["in", "30m"], now).unwrap();
        assert_eq!(w.kind, Kind::Once);
        assert_eq!(w.consumed, 2);
        let w = parse_when(&["every", "2h"], now).unwrap();
        assert_eq!((w.kind, w.spec.as_str()), (Kind::Interval, "7200"));
        let w = parse_when(&["cron", "0", "9", "*", "*", "1-5"], now).unwrap();
        assert_eq!(
            (w.kind, w.spec.as_str(), w.consumed),
            (Kind::Cron, "0 9 * * 1-5", 6)
        );
        let w = parse_when(&["at", "2026-06-15T15:00:00"], now).unwrap();
        assert_eq!(w.kind, Kind::Once);
        // 秒可省:少写两位不该整条报错(卡片走 date_picker+picker_time,文本侧也得容得下人话写法)。
        let no_secs = parse_when(&["at", "2026-06-15T15:00"], now).unwrap();
        assert_eq!(no_secs.spec, w.spec);
        // 带时区偏移的 RFC3339 仍走原路。
        assert!(parse_when(&["at", "2026-06-15T15:00:00+08:00"], now).is_ok());
        // 真正的垃圾还是要报错,别把「宽容」滑成「什么都收」。
        assert!(parse_when(&["at", "2026-06-15"], now).is_err());
        assert!(parse_when(&["at", "明天下午"], now).is_err());
    }

    #[test]
    fn parse_when_rejects_garbage() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        assert!(parse_when(&["in", "banana"], now).is_err());
        assert!(parse_when(&["cron", "0", "9"], now).is_err());
        assert!(parse_when(&[], now).is_err());
    }

    #[test]
    fn parse_dur_secs_covers_normal_and_out_of_range() {
        // (输入, 期望):Some=秒数,None=应报错不 panic。
        let cases: &[(&str, Option<i64>)] = &[
            ("30m", Some(1800)),
            ("2h", Some(7200)),
            ("1d", Some(86400)),
            ("90s", Some(90)),
            ("0s", Some(0)),
            ("-30m", Some(-1800)),
            ("3650d", Some(MAX_DUR_SECS)),   // 恰好上界
            ("3651d", None),                 // 上界 +1 天
            ("-3651d", None),                // 负向同样受限
            ("9223372036854775807d", None),  // i64::MAX * 86400 → checked_mul 溢出
            ("-9223372036854775808s", None), // i64::MIN,禁止 abs() 反溢出
            ("1000000000d", None),           // 任务描述里的毒输入
            ("banana", None),
            ("30x", None),
            ("", None),
            ("m", None),
        ];
        for (input, want) in cases {
            let got = parse_dur_secs(input);
            match want {
                Some(secs) => assert_eq!(got.as_ref().ok(), Some(secs), "输入 {input}"),
                None => assert!(got.is_err(), "输入 {input} 应报错,实际 {got:?}"),
            }
        }
    }

    /// `split_at(len-1)` 按字节切,末字符是多字节时会踩 char boundary panic。
    #[test]
    fn parse_dur_secs_rejects_multibyte_unit_without_panic() {
        for s in ["30秒", "5分", "秒"] {
            assert!(parse_dur_secs(s).is_err(), "输入 {s} 应报错");
        }
    }

    #[test]
    fn parse_when_in_rejects_out_of_range_duration() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        assert!(parse_when(&["in", "1000000000d"], now).is_err());
        assert!(parse_when(&["in", "9223372036854775807s"], now).is_err());
        assert!(parse_when(&["in", "3651d"], now).is_err());
        // 上界内仍要正常工作(别把合法输入一起毙了)。
        assert!(parse_when(&["in", "3650d"], now).is_ok());
        assert!(parse_when(&["in", "10000m"], now).is_ok());
    }

    #[test]
    fn parse_when_every_enforces_both_bounds() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        assert!(parse_when(&["every", "59s"], now).is_err()); // 下界
        assert!(parse_when(&["every", "60s"], now).is_ok());
        assert!(parse_when(&["every", "3650d"], now).is_ok()); // 上界
        assert!(parse_when(&["every", "3651d"], now).is_err());
        assert!(parse_when(&["every", "1000000000d"], now).is_err());
    }

    /// 坏数据可能已落盘(旧版本无上界)。读盘路径只能静默让路,不许 panic 掉 scheduler 线程。
    #[test]
    fn due_plan_interval_skips_overflowing_spec() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        for spec in [
            "9223372036854775807",
            "9223372036854775806",
            "86400000000000000",
        ] {
            let mut s = sched(Kind::Interval, spec);
            s.last_fired = Some((now - chrono::Duration::hours(5)).to_rfc3339());
            let p = due_plan(&s, now);
            assert!(!p.fire_now, "spec {spec} 不该 fire");
            assert!(!p.disable, "spec {spec} 不该 disable(写入侧才是治本处)");
        }
    }

    /// `at` 的 RFC3339 解析只吃 4 位年份 → 落盘时刻最远 9999 年,离 `DateTime<Local>` 上界
    /// (约 262143 年)还有 25 万年余量。这是 `cron::next_after` 里 `from + 366d` 不会溢出的前提
    /// (它的 `from` 只可能来自 last_fired/created_at,两者都是本函数或系统时钟产的 RFC3339)。
    #[test]
    fn parse_when_at_rejects_out_of_range_year() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        for ts in [
            "+262143-01-01T00:00:00Z",
            "999999-01-01T00:00:00Z",
            "262143-01-01T00:00:00Z",
        ] {
            assert!(parse_when(&["at", ts], now).is_err(), "时刻 {ts} 应报错");
        }
        assert!(parse_when(&["at", "9999-01-01T00:00:00Z"], now).is_ok());
    }

    #[test]
    fn due_plan_once_fires_then_disables() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        let mut s = sched(
            Kind::Once,
            &(now - chrono::Duration::minutes(1)).to_rfc3339(),
        );
        let p = due_plan(&s, now);
        assert!(p.fire_now && p.disable);
        s.last_fired = Some(now.to_rfc3339());
        assert!(!due_plan(&s, now).fire_now);
    }

    #[test]
    fn due_plan_interval_catches_up_once() {
        use chrono::{Duration, Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        let mut s = sched(Kind::Interval, "3600");
        s.last_fired = Some((now - Duration::hours(5)).to_rfc3339());
        let p = due_plan(&s, now);
        assert!(p.fire_now && !p.disable);
        assert!(p.next_fire.unwrap() > now);
    }

    #[test]
    fn due_plan_disabled_never_fires() {
        use chrono::{Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap();
        let mut s = sched(
            Kind::Once,
            &(now - chrono::Duration::minutes(1)).to_rfc3339(),
        );
        s.enabled = false;
        assert!(!due_plan(&s, now).fire_now);
    }

    fn sched(kind: Kind, spec: &str) -> Schedule {
        Schedule {
            id: "SCH-1".into(),
            kind,
            spec: spec.into(),
            agent: "a".into(),
            chat_id: "c".into(),
            chat_type: "p2p".into(),
            prompt: "p".into(),
            created_by: "ou".into(),
            created_at: "2026-06-14 00:00:00".into(),
            output: ScheduleOutput::Progress,
            context: ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        }
    }
}

#[cfg(test)]
mod remind_syntax_tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 21, 10, 0, 0).unwrap()
    }

    fn when(expr: &str) -> Result<When, String> {
        let tokens: Vec<&str> = expr.split_whitespace().collect();
        parse_when(&tokens, now())
    }

    /// 三种新节律落成的 cron 必须与卡片那条路**逐字节一致** —— 同一个意思在
    /// `/schedules` 上显示成两种样子,就是两套语义在打架。
    #[test]
    fn daily_weekday_and_weekly_land_on_the_same_cron_the_card_produces() {
        let daily = when("daily 09:00").unwrap();
        assert_eq!(daily.kind, Kind::Cron);
        assert_eq!(daily.spec, "0 9 * * *");

        let weekday = when("weekday 09:30").unwrap();
        assert_eq!(weekday.spec, "30 9 * * 1-5");

        let weekly = when("weekly mon 10:00").unwrap();
        assert_eq!(weekly.spec, "0 10 * * 1");
    }

    /// 拼出来的 cron 要能被自己的解析器认回去,并且能渲成人话 —— 后者是 `/schedules`
    /// 列表的显示来源,渲不出就会退化成裸表达式。
    #[test]
    fn every_generated_cron_round_trips_and_speaks_chinese() {
        for expr in ["daily 09:00", "weekday 18:00", "weekly mon,fri 10:30"] {
            let spec = when(expr).unwrap().spec;
            cron::parse_cron(&spec).unwrap_or_else(|e| panic!("{expr} → {spec} 解析失败:{e}"));
            assert!(
                cron::human(&spec).is_some(),
                "{expr} → {spec} 渲不出人话,列表里会是裸表达式"
            );
        }
    }

    #[test]
    fn weekday_names_accept_the_spellings_people_actually_type() {
        for (expr, expected) in [
            ("weekly mon 10:00", "0 10 * * 1"),
            ("weekly Monday 10:00", "0 10 * * 1"),
            ("weekly MON 10:00", "0 10 * * 1"),
            ("weekly 周一 10:00", "0 10 * * 1"),
            ("weekly 1 10:00", "0 10 * * 1"),
            ("weekly sun 10:00", "0 10 * * 0"),
            ("weekly 周日 10:00", "0 10 * * 0"),
        ] {
            assert_eq!(when(expr).unwrap().spec, expected, "输入:{expr}");
        }
    }

    /// 多天要去重排序:`mon,mon` 落成 `1,1` 会渲成「每周一、周一」。
    #[test]
    fn multiple_weekdays_are_sorted_and_deduped() {
        assert_eq!(when("weekly fri,mon 10:00").unwrap().spec, "0 10 * * 1,5");
        assert_eq!(when("weekly mon,mon 10:00").unwrap().spec, "0 10 * * 1");
    }

    /// 原有四种节律不能因为新增而漂移。
    #[test]
    fn the_original_rhythms_still_parse_unchanged() {
        assert_eq!(when("every 2h").unwrap().kind, Kind::Interval);
        assert_eq!(when("every 2h").unwrap().spec, "7200");
        assert_eq!(when("in 30m").unwrap().kind, Kind::Once);
        assert_eq!(when("cron 0 9 * * 1-5").unwrap().spec, "0 9 * * 1-5");
        assert_eq!(when("at 2026-12-01T09:00").unwrap().kind, Kind::Once);
    }

    /// 报错要说清合法集合 —— 调用方是 agent,错误消息就是它的文档。
    #[test]
    fn errors_name_the_legal_rhythms_and_the_expected_shape() {
        let unknown = when("tomorrow 09:00").unwrap_err();
        for rhythm in ["in", "at", "every", "daily", "weekday", "weekly", "cron"] {
            assert!(unknown.contains(rhythm), "错误没列出 {rhythm}:{unknown}");
        }
        assert!(when("daily").unwrap_err().contains("09:00"));
        assert!(when("daily 25:00").unwrap_err().contains("越界"));
        assert!(when("weekly funday 10:00").unwrap_err().contains("funday"));
    }

    /// 越界与坏输入一律 Err,绝不 panic:这条命令由 agent 拼参数调用,输入本就不可信。
    #[test]
    fn malformed_input_is_rejected_without_panicking() {
        for expr in [
            "daily 9",
            "daily :30",
            "weekly 10:00",
            "weekly mon",
            "weekly mon 24:00",
            "daily 09:60",
            "weekly ,, 10:00",
        ] {
            assert!(when(expr).is_err(), "本该拒绝:{expr}");
        }
    }
}
