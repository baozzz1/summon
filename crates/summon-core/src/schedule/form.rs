//! 飞书卡片表单 → `(Kind, spec)` 的纯转换层。
//!
//! why 单独成文件:`form_value` 的取值形态由飞书客户端决定,**本仓没有真机证据**(四个新控件
//! 首次引入)。把「宽容读取 + fail-closed 校验」收在一个纯函数模块里,真机形态与预期不符时
//! 只改这里、且单测能先复现。

use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::Value;

use super::Kind;

/// 卡片第一步的频率按钮。
///
/// **只存在于卡片交互层**——落盘仍是 `Kind` + `spec`,不为它新增持久化形态。「每个工作日」就是
/// 「每周一到五」的预设;`After`(过多久)落成的也是普通的 `Once` + 绝对时刻,只是把「现在 + n」
/// 这步算术从用户脑子里挪进代码。单列成按钮只为省掉最高频的那几次操作,都不是新节律。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardFreq {
    Once,
    /// 「过多久」→ 仍是一次性,但用相对时长表达。定一个「20 分钟后提醒我」不该逼人去滚日期和
    /// 时刻选择器——那是把机器该做的减法推给了用户。
    After,
    Daily,
    Weekday,
    Weekly,
    Every,
}

impl CardFreq {
    /// 编码进按钮 `value.kind` 的稳定串。改动即破坏在途卡片,不要动。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::After => "after",
            Self::Daily => "daily",
            Self::Weekday => "weekday",
            Self::Weekly => "weekly",
            Self::Every => "every",
        }
    }

    /// 解析按钮 `value.kind`。未识别 → `None`,调用方回一句可读错误(卡片是旧版本或被改过)。
    pub fn from_kind(s: &str) -> Option<Self> {
        match s {
            "once" => Some(Self::Once),
            "after" => Some(Self::After),
            "daily" => Some(Self::Daily),
            "weekday" => Some(Self::Weekday),
            "weekly" => Some(Self::Weekly),
            "every" => Some(Self::Every),
            _ => None,
        }
    }

    /// 参数卡标题里的人话。
    pub fn label(self) -> &'static str {
        match self {
            Self::Once => summon_i18n::text!("r3ad144f12c"),
            Self::After => summon_i18n::text!("re0f10b3654"),
            Self::Daily => summon_i18n::text!("reea1694c23"),
            Self::Weekday => summon_i18n::text!("rb9437afd86"),
            Self::Weekly => summon_i18n::text!("r92845d3b53"),
            Self::Every => summon_i18n::text!("r02431769cf"),
        }
    }
}

/// 参数卡的预填值。新建时全空;编辑时由 [`Prefill::of_schedule`] 从盘上那条反推。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Prefill {
    pub prompt: String,
    pub output: super::ScheduleOutput,
    /// `HH:MM`;空 = 不预填。
    pub time: String,
    pub days: Vec<u32>,
    /// `YYYY-MM-DD`;空 = 不预填。
    pub date: String,
    pub every_n: String,
    /// `m` / `h` / `d`;空 = 不预填。
    pub every_unit: String,
}

impl Prefill {
    /// 编辑已有定时器时,反推「该画哪张参数卡、各控件填什么」。
    ///
    /// 识别不了的自定义 cron(`*/15 9-18 * * *` 这类)返回 `None`——调用方据此引导用户重选频率,
    /// 而不是拿一张画错的卡去覆盖人家亲手写的表达式。**这是「不猜」原则在编辑路径上的落点。**
    pub fn of_schedule(s: &super::Schedule) -> Option<(CardFreq, Self)> {
        let base = Prefill {
            prompt: s.prompt.clone(),
            output: s.output,
            ..Default::default()
        };
        match s.kind {
            Kind::Once => {
                let t = super::parse_rfc3339(&s.spec)?;
                Some((
                    CardFreq::Once,
                    Prefill {
                        date: t.format("%Y-%m-%d").to_string(),
                        time: t.format("%H:%M").to_string(),
                        ..base
                    },
                ))
            }
            Kind::Interval => {
                let secs: i64 = s.spec.parse().ok().filter(|n| *n > 0)?;
                let (n, unit) = split_interval(secs)?;
                Some((
                    CardFreq::Every,
                    Prefill {
                        every_n: n.to_string(),
                        every_unit: unit.to_string(),
                        ..base
                    },
                ))
            }
            Kind::Cron => {
                let (dow, hour, minute) = super::cron::plain_weekly(&s.spec)?;
                let freq = match dow.as_slice() {
                    d if d.len() == 7 => CardFreq::Daily,
                    [1, 2, 3, 4, 5] => CardFreq::Weekday,
                    _ => CardFreq::Weekly,
                };
                Some((
                    freq,
                    Prefill {
                        time: format!("{hour:02}:{minute:02}"),
                        days: dow,
                        ..base
                    },
                ))
            }
        }
    }
}

impl Prefill {
    /// 把用户**刚提交的表单原样收回来**,用于校验失败时重画参数卡。
    ///
    /// why 必须有:校验失败若把整张卡换成一句错误文本,用户已填的正文 / 时间 / 星期就全没了,
    /// 只能从 `/remind` 重来一遍。填错时间是最常见的一次失误,代价不该是重填全部。
    /// 这里不做任何校验——它的职责就是「原样回显」,对错交给 [`to_spec`]。
    pub fn from_form(form: &Value) -> Self {
        Prefill {
            prompt: prompt(form),
            output: output(form).unwrap_or_default(),
            time: text(form, field::TIME),
            days: days(form),
            date: text(form, field::DATE),
            every_n: text(form, field::EVERY_N),
            every_unit: text(form, field::EVERY_UNIT),
        }
    }
}

/// 秒 → `(数量, 单位)`,取能整除的最大单位。`5400` 给 `(90, "m")` 而不是 `(1, "h")`。
///
/// 不能被 60 整除(`/remind every 90s` 排出来的)返回 `None`:卡片的单位只到分钟,**预填必须能
/// 原样存回同一条 spec**——否则用户只想改正文,提交后节律被悄悄取整,这是最难被发现的一类坏账。
fn split_interval(secs: i64) -> Option<(i64, &'static str)> {
    match secs {
        s if s % 86_400 == 0 => Some((s / 86_400, "d")),
        s if s % 3_600 == 0 => Some((s / 3_600, "h")),
        s if s % 60 == 0 => Some((s / 60, "m")),
        _ => None,
    }
}

/// 表单里所有可能出现的字段名。卡片渲染与解析共用同一份常量,拼错即编译不过。
pub mod field {
    pub const PROMPT: &str = "prompt";
    pub const DAYS: &str = "days";
    pub const TIME: &str = "time";
    pub const DATE: &str = "date";
    pub const EVERY_N: &str = "every_n";
    pub const EVERY_UNIT: &str = "every_unit";
    pub const OUTPUT: &str = "output";
}

/// 从回调 value 里取出表单区(`form_value`)。缺失即空对象——调用方随后会因必填校验拒绝,
/// 不需要在这里区分「没有表单」与「表单空」。
pub fn form_of(value: &Value) -> Value {
    value
        .get("form_value")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()))
}

/// 宽容取文本:字符串原样,数字转成串(飞书对 `input` 的数字内容可能给 number),其余 → 空串。
fn text(form: &Value, key: &str) -> String {
    match form.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 任务正文。空正文由调用方拒(定时器没有内容就没有意义)。
pub fn prompt(form: &Value) -> String {
    text(form, field::PROMPT)
}

/// 旧卡片没有 output 字段时保持完整过程；未知值拒绝，避免被篡改的回调静默换语义。
pub fn output(form: &Value) -> Result<super::ScheduleOutput, String> {
    let value = text(form, field::OUTPUT);
    if value.is_empty() {
        return Ok(super::ScheduleOutput::Progress);
    }
    value.parse()
}

/// 宽容取多选:同时接受 JSON 数组(元素为串或数)与逗号分隔串。
///
/// why 不赌单一形态:`multi_select_static` 的 `form_value` 取值本仓无真机证据,文档也没写死。
/// 两种形态都吃下,比赌错一种然后在真机上静默丢掉用户的选择要好。结果排序去重,保证
/// `1,3` 与 `3,1,3` 落成同一条 spec。
fn days(form: &Value) -> Vec<u32> {
    let mut out: Vec<u32> = match form.get(field::DAYS) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => s.trim().parse().ok(),
                Value::Number(n) => n.as_u64().map(|n| n as u32),
                _ => None,
            })
            .collect(),
        Some(Value::String(s)) => s.split(',').filter_map(|p| p.trim().parse().ok()).collect(),
        Some(Value::Number(n)) => n.as_u64().map(|n| vec![n as u32]).unwrap_or_default(),
        _ => Vec::new(),
    };
    out.retain(|d| *d <= 6);
    out.sort_unstable();
    out.dedup();
    out
}

/// 宽容解析 `HH:mm`。同时接受 `HH:mm:ss`、个位数不补零(`9:5`),以及**尾部的时区后缀**
/// (`10:30 +0800`)。
///
/// why 要吃时区后缀:`picker_time` 的真机回传形态本仓无证据,而它若带上偏移量,四种带时刻的频率
/// 会**全部**建不出来——一个字符串切法就能让整个功能归零。这里只取第一段空白前的内容:时刻本身
/// 是本地墙钟(cron 按本地时区判定),偏移量对它没有意义,丢掉是正确的而不是将就。
pub(super) fn hhmm(raw: &str) -> Result<(u32, u32), String> {
    let bad = || summon_i18n::format!("r29f9636706");
    let head = raw.split_whitespace().next().unwrap_or("");
    let mut parts = head.split(':');
    let (Some(h), Some(m)) = (parts.next(), parts.next()) else {
        return Err(bad());
    };
    let h: u32 = h.trim().parse().map_err(|_| bad())?;
    let m: u32 = m.trim().parse().map_err(|_| bad())?;
    if h > 23 || m > 59 {
        return Err(summon_i18n::format!("r4ce9792c5e"));
    }
    Ok((h, m))
}

/// 宽容解析日期:`YYYY-MM-DD`,并容忍后面跟着时间部分(`2026-08-04 09:00` / RFC3339)。
fn ymd(raw: &str) -> Result<NaiveDate, String> {
    let head = raw
        .split([' ', 'T'])
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string();
    NaiveDate::parse_from_str(&head, "%Y-%m-%d").map_err(|_| summon_i18n::format!("rc833db2a9d"))
}

/// 间隔单位 → 秒。接受 select 的稳定值(`m`/`h`/`d`)与中文回显,未识别 → `None`。
fn unit_secs(raw: &str) -> Option<i64> {
    match raw.trim() {
        "m" | "min" | summon_i18n::source!("rbd957bc497") | summon_i18n::source!("rb6a993c256") => {
            Some(60)
        }
        "h"
        | "hour"
        | summon_i18n::source!("r3b6fefc50f")
        | summon_i18n::source!("r7d58b6422c") => Some(3600),
        "d" | "day" | summon_i18n::source!("r49da61ceee") => Some(86_400),
        _ => None,
    }
}

/// 卡片表单 → `(Kind, spec)`。**这是写入侧的最后一道闸**:控件的 `required` 只是前端提示,
/// 不可信,所有必填与边界在这里再判一次,错了给可读中文,绝不落一条永不触发(或立刻雪崩)的定时器。
pub fn to_spec(
    freq: CardFreq,
    form: &Value,
    now: DateTime<Local>,
) -> Result<(Kind, String), String> {
    match freq {
        CardFreq::Once => {
            let date = ymd(&text(form, field::DATE))?;
            let (h, m) = hhmm(&text(form, field::TIME))?;
            let naive = date.and_time(
                NaiveTime::from_hms_opt(h, m, 0).ok_or(summon_i18n::text!("r9df6824c9e"))?,
            );
            // 夏令时空洞里的本地时刻不存在,`earliest()` 会给 None——直接拒,别默默顺延。
            let when = Local
                .from_local_datetime(&naive)
                .earliest()
                .ok_or_else(|| summon_i18n::text!("r6771fadcd8").to_string())?;
            if when <= now {
                return Err(summon_i18n::format!(
                    "r4e1e0ce364",
                    when.format("%Y-%m-%d %H:%M")
                ));
            }
            Ok((Kind::Once, when.to_rfc3339()))
        }
        CardFreq::After => {
            // 复用 every_n / every_unit 两个字段:形态与「每隔」完全一样,只是落成一次性。
            // 不新增字段 = 换频率往返时值能互相带过去,也少一组要维护的常量。
            let secs = duration_secs(form)?;
            // 下界 60s:tick 是 30s,更短的「几秒后」在这套节拍下没有意义,写了也是骗人。
            if secs < 60 {
                return Err(summon_i18n::text!("r2cf8367a35").to_string());
            }
            let when = chrono::Duration::try_seconds(secs)
                .and_then(|d| now.checked_add_signed(d))
                .ok_or_else(|| summon_i18n::text!("r56ab4fada0").to_string())?;
            Ok((Kind::Once, when.to_rfc3339()))
        }
        CardFreq::Daily => {
            let (h, m) = hhmm(&text(form, field::TIME))?;
            Ok((Kind::Cron, cron_spec(m, h, "*")))
        }
        CardFreq::Weekday => {
            let (h, m) = hhmm(&text(form, field::TIME))?;
            Ok((Kind::Cron, cron_spec(m, h, "1-5")))
        }
        CardFreq::Weekly => {
            let picked = days(form);
            if picked.is_empty() {
                return Err(summon_i18n::text!("r7013737b69").to_string());
            }
            let (h, m) = hhmm(&text(form, field::TIME))?;
            Ok((
                Kind::Cron,
                cron_spec(m, h, &super::cron::dow_field(&picked)),
            ))
        }
        CardFreq::Every => {
            let secs = duration_secs(form)?;
            // 与 `/remind every` 同一条下界:scheduler ~30s tick,比 60s 更密的间隔没有意义。
            // 当前单位集(分/时/天)最小就是 60s,这条**恒不触发**——留着是守 `unit_secs` 的:
            // 哪天加了「秒」,下界不会跟着漏掉。
            if secs < 60 {
                return Err(summon_i18n::text!("r4ae5e65744").to_string());
            }
            Ok((Kind::Interval, secs.to_string()))
        }
    }
}

/// `every_n` + `every_unit` → 秒。「每隔」与「过多久」共用——两者的输入形态本就一样,
/// 只是一个落成周期、一个落成一次性。
fn duration_secs(form: &Value) -> Result<i64, String> {
    let raw_n = text(form, field::EVERY_N);
    let n: i64 = raw_n
        .parse()
        .map_err(|_| summon_i18n::format!("r9ef503595e"))?;
    if n <= 0 {
        return Err(summon_i18n::text!("rf3233d8fef").to_string());
    }
    let unit = text(form, field::EVERY_UNIT);
    let mult = unit_secs(&unit).ok_or_else(|| summon_i18n::format!("r633330a2d9"))?;
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| summon_i18n::text!("r82d0ada368").to_string())?;
    if secs > super::MAX_DUR_SECS {
        return Err(summon_i18n::text!("r82d0ada368").to_string());
    }
    Ok(secs)
}

/// 拼 5 段 cron 并**当场自校验**:拼错了要在这里炸,而不是落盘后由 `due_plan` 静默永不触发。
fn cron_spec(minute: u32, hour: u32, dow: &str) -> String {
    let spec = format!("{minute} {hour} * * {dow}");
    debug_assert!(
        super::cron::parse_cron(&spec).is_ok(),
        "{}",
        summon_i18n::format!("r17c0c304fb")
    );
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 3, 12, 0, 0).unwrap()
    }

    #[test]
    fn freq_str_round_trips() {
        for f in [
            CardFreq::Once,
            CardFreq::Daily,
            CardFreq::Weekday,
            CardFreq::Weekly,
            CardFreq::Every,
        ] {
            assert_eq!(CardFreq::from_kind(f.as_str()), Some(f));
        }
        assert_eq!(CardFreq::from_kind("monthly"), None);
    }

    #[test]
    fn daily_and_weekday_build_cron() {
        let form = json!({"time": "09:00"});
        assert_eq!(
            to_spec(CardFreq::Daily, &form, now()).unwrap(),
            (Kind::Cron, "0 9 * * *".to_string())
        );
        assert_eq!(
            to_spec(CardFreq::Weekday, &form, now()).unwrap(),
            (Kind::Cron, "0 9 * * 1-5".to_string())
        );
    }

    #[test]
    fn weekly_accepts_both_multiselect_shapes() {
        // 真机形态未知:数组(串/数)与逗号串都必须落成同一条 spec。
        let expect = (Kind::Cron, "30 10 * * 1,3".to_string());
        for days in [
            json!(["1", "3"]),
            json!([1, 3]),
            json!("1,3"),
            json!(["3", "1", "3"]),
        ] {
            let form = json!({"days": days, "time": "10:30"});
            assert_eq!(to_spec(CardFreq::Weekly, &form, now()).unwrap(), expect);
        }
    }

    #[test]
    fn weekly_rejects_empty_selection() {
        // 控件 required 不可信:空选必须在服务端拒,否则会落一条 `* * * * ` 之类的坏 spec。
        for days in [json!([]), json!(""), json!(null)] {
            let form = json!({"days": days, "time": "10:30"});
            assert!(to_spec(CardFreq::Weekly, &form, now()).is_err());
        }
        assert!(to_spec(CardFreq::Weekly, &json!({"time": "10:30"}), now()).is_err());
    }

    #[test]
    fn time_parsing_is_lenient_but_bounded() {
        let mk = |t: serde_json::Value| json!({"time": t});
        // HH:mm:ss(picker_time 可能带秒)与不补零都要吃下。
        assert_eq!(
            to_spec(CardFreq::Daily, &mk(json!("09:05:00")), now())
                .unwrap()
                .1,
            "5 9 * * *"
        );
        assert_eq!(
            to_spec(CardFreq::Daily, &mk(json!("9:5")), now())
                .unwrap()
                .1,
            "5 9 * * *"
        );
        // picker_time 若带时区后缀,不能让四种带时刻的频率全线建不出来。
        assert_eq!(
            to_spec(CardFreq::Daily, &mk(json!("10:30 +0800")), now())
                .unwrap()
                .1,
            "30 10 * * *"
        );
        assert_eq!(
            to_spec(CardFreq::Daily, &mk(json!("10:30:00 +08:00")), now())
                .unwrap()
                .1,
            "30 10 * * *"
        );
        // 越界/垃圾必须报错而不是取模或截断。
        assert!(to_spec(CardFreq::Daily, &mk(json!("24:00")), now()).is_err());
        assert!(to_spec(CardFreq::Daily, &mk(json!("09:60")), now()).is_err());
        assert!(to_spec(CardFreq::Daily, &mk(json!("上午九点")), now()).is_err());
        assert!(to_spec(CardFreq::Daily, &mk(json!("")), now()).is_err());
    }

    #[test]
    fn once_requires_future_instant() {
        let past = json!({"date": "2026-08-03", "time": "09:00"});
        assert!(to_spec(CardFreq::Once, &past, now()).is_err());
        let future = json!({"date": "2026-08-04", "time": "09:00"});
        let (kind, spec) = to_spec(CardFreq::Once, &future, now()).unwrap();
        assert_eq!(kind, Kind::Once);
        assert!(spec.starts_with("2026-08-04T09:00:00"));
        // date_picker 若回带时间部分,取日期段即可,不该整条解析失败。
        let with_time = json!({"date": "2026-08-04 00:00:00", "time": "09:00"});
        assert_eq!(to_spec(CardFreq::Once, &with_time, now()).unwrap().1, spec);
    }

    #[test]
    fn every_enforces_floor_and_ceiling() {
        let mk = |n: serde_json::Value, u: &str| json!({"every_n": n, "every_unit": u});
        assert_eq!(
            to_spec(CardFreq::Every, &mk(json!("2"), "h"), now()).unwrap(),
            (Kind::Interval, "7200".to_string())
        );
        // 数字形态的 input 也要吃下。
        assert_eq!(
            to_spec(CardFreq::Every, &mk(json!(2), "小时"), now())
                .unwrap()
                .1,
            "7200"
        );
        // 非正数被拒;下界 60s 由「最小单位是分钟」保证,不靠用户守规矩。
        assert!(to_spec(CardFreq::Every, &mk(json!("0"), "h"), now()).is_err());
        assert!(to_spec(CardFreq::Every, &mk(json!("-1"), "h"), now()).is_err());
        assert_eq!(
            to_spec(CardFreq::Every, &mk(json!("1"), "m"), now())
                .unwrap()
                .1,
            "60"
        );
        // 上界 3650 天,与 `parse_dur_secs` 同一条线。
        assert!(to_spec(CardFreq::Every, &mk(json!("4000"), "d"), now()).is_err());
        assert!(to_spec(CardFreq::Every, &mk(json!("abc"), "h"), now()).is_err());
        // 未登记的单位串必须拒,不能默默当成某个默认值。
        assert!(to_spec(CardFreq::Every, &mk(json!("30"), "s"), now()).is_err());
        assert!(to_spec(CardFreq::Every, &mk(json!("30"), ""), now()).is_err());
    }

    fn sched(kind: Kind, spec: &str) -> super::super::Schedule {
        super::super::Schedule {
            id: "SCH-1".into(),
            kind,
            spec: spec.into(),
            agent: "a".into(),
            chat_id: "c".into(),
            chat_type: "group".into(),
            prompt: "跑日报".into(),
            created_by: "ou".into(),
            created_at: "2026-08-03 12:00:00".into(),
            output: super::super::ScheduleOutput::Progress,
            context: super::super::ScheduleContext::Resume,
            enabled: true,
            last_fired: None,
            revision: 0,
        }
    }

    /// 编辑路径的核心性质:**预填 → 原样提交 = 同一条 spec**。
    /// 破了它,用户「只改了正文」的一次提交会把节律一并改掉,而且没有任何提示。
    #[test]
    fn prefill_round_trips_back_to_same_spec() {
        let cases = [
            (Kind::Cron, "0 9 * * *"),
            (Kind::Cron, "0 9 * * 1-5"),
            (Kind::Cron, "30 10 * * 1,3"),
            (Kind::Interval, "7200"),
            (Kind::Interval, "5400"),
            (Kind::Interval, "86400"),
        ];
        for (kind, spec) in cases {
            let s = sched(kind, spec);
            let (freq, pre) =
                Prefill::of_schedule(&s).unwrap_or_else(|| panic!("{spec} 应该能反推出参数卡"));
            let form = json!({
                "prompt": pre.prompt,
                "days": pre.days,
                "time": pre.time,
                "date": pre.date,
                "every_n": pre.every_n,
                "every_unit": pre.every_unit,
            });
            let (back_kind, back_spec) = to_spec(freq, &form, now()).unwrap();
            assert_eq!(
                (back_kind, back_spec.as_str()),
                (kind, spec),
                "spec 漂移: {spec}"
            );
        }
    }

    #[test]
    fn prefill_declines_shapes_the_card_cannot_express() {
        // 自定义 cron 与非整分间隔:宁可让用户重选频率,也不画一张会改坏节律的卡。
        assert!(Prefill::of_schedule(&sched(Kind::Cron, "*/15 9-18 * * *")).is_none());
        assert!(Prefill::of_schedule(&sched(Kind::Cron, "0 9 1 * *")).is_none());
        assert!(Prefill::of_schedule(&sched(Kind::Interval, "90")).is_none());
        assert!(Prefill::of_schedule(&sched(Kind::Interval, "0")).is_none());
        assert!(Prefill::of_schedule(&sched(Kind::Once, "坏了")).is_none());
    }

    #[test]
    fn prefill_of_once_keeps_local_wall_clock() {
        let s = sched(Kind::Once, "2026-08-04T09:30:00+08:00");
        let (freq, pre) = Prefill::of_schedule(&s).unwrap();
        assert_eq!(freq, CardFreq::Once);
        // 落盘是带时区的瞬时,预填给的是本地墙钟——跨时区跑时两者不必相等,这里只钉住格式。
        assert_eq!(pre.date.len(), 10);
        assert_eq!(pre.time.len(), 5);
        assert_eq!(pre.prompt, "跑日报");
    }

    /// 校验失败重画卡时,用户已填的每一项都要原样回来——包括那个填错的字段本身
    /// (让人看见自己填了什么才好改)。
    #[test]
    fn from_form_echoes_everything_back() {
        let form = json!({
            "prompt": "  写周报 ",
            "time": "25:00",
            "days": ["3", "1"],
            "date": "2026-08-04",
            "every_n": "2",
            "every_unit": "h",
            "output": "result",
        });
        let pre = Prefill::from_form(&form);
        assert_eq!(pre.prompt, "写周报");
        assert_eq!(
            pre.time, "25:00",
            "填错的值也要回显,否则用户不知道自己填了啥"
        );
        assert_eq!(pre.days, vec![1, 3]);
        assert_eq!(pre.date, "2026-08-04");
        assert_eq!(pre.every_n, "2");
        assert_eq!(pre.every_unit, "h");
        assert_eq!(pre.output, super::super::ScheduleOutput::Result);
        assert_eq!(Prefill::from_form(&json!({})), Prefill::default());
    }

    /// 「过多久」落成的是**普通的一次性 + 绝对时刻**,不新增持久化形态——
    /// 「现在 + n」这步减法由代码做,不该推给用户去滚日期时刻选择器。
    #[test]
    fn after_resolves_relative_duration_into_absolute_once() {
        let mk = |n: serde_json::Value, u: &str| json!({"every_n": n, "every_unit": u});
        let (kind, spec) = to_spec(CardFreq::After, &mk(json!("20"), "m"), now()).unwrap();
        assert_eq!(kind, Kind::Once);
        // now = 2026-08-03 12:00 → 12:20。
        assert!(spec.starts_with("2026-08-03T12:20:00"), "spec={spec}");

        let (_, spec) = to_spec(CardFreq::After, &mk(json!("2"), "h"), now()).unwrap();
        assert!(spec.starts_with("2026-08-03T14:00:00"), "spec={spec}");
        let (_, spec) = to_spec(CardFreq::After, &mk(json!("1"), "d"), now()).unwrap();
        assert!(spec.starts_with("2026-08-04T12:00:00"), "spec={spec}");

        // 与「每隔」共用字段,所以换频率往返时值能互相带过去。
        assert_eq!(
            to_spec(CardFreq::Every, &mk(json!("2"), "h"), now()).unwrap(),
            (Kind::Interval, "7200".to_string())
        );

        // tick 是 30s,「几秒后」在这套节拍下是骗人的。
        assert!(to_spec(CardFreq::After, &mk(json!("0"), "m"), now()).is_err());
        assert!(to_spec(CardFreq::After, &mk(json!("-5"), "m"), now()).is_err());
        assert!(to_spec(CardFreq::After, &mk(json!("abc"), "m"), now()).is_err());
        assert!(to_spec(CardFreq::After, &mk(json!("4000"), "d"), now()).is_err());
    }

    #[test]
    fn prompt_is_trimmed() {
        assert_eq!(prompt(&json!({"prompt": "  跑日报  "})), "跑日报");
        assert_eq!(prompt(&json!({})), "");
    }

    #[test]
    fn form_of_tolerates_missing_form_value() {
        assert_eq!(prompt(&form_of(&json!({"action": "schedule_create"}))), "");
        assert_eq!(
            prompt(&form_of(&json!({"form_value": {"prompt": "x"}}))),
            "x"
        );
    }
}
