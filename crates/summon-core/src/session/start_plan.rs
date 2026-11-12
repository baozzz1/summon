//! 起会话(`Session::ensure_started`)的 resume/冷启判定。
//!
//! 抽成纯函数是因为这条判定此前只被 4 个 `#[ignore]` 真机测试覆盖:把判定取反,默认门禁全绿,
//! 而取反的后果是「resume 成功的会话被当 stale 杀掉重冷启」(每次 daemon 重启后第一条消息丢光
//! 上下文)或「resume 失败被当成功」(chat 线程挂在起不来的 tmux 上)。claude 是主力 CLI,
//! 这是核心路径,必须有非 ignored 的机器门禁钉住。

/// `ensure_started` 探测完成后要执行的动作。
///
/// 注意 `KillStaleThenCold` 里的 kill 杀的是**本次刚起、且已确认没续接成功的残壳**
/// (stale id 让 CLI 报错退出留下的空会话),不是用户正在用的活会话——「会话中断只有三条
/// 合法途径」那条红线管的是后者。这里只做分类,不改这条既有语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPlan<'a> {
    /// tmux 会话本就活着,直接复用其内存上下文。
    AlreadyRunning,
    /// resume 命令已把会话续起来了,复用记录里的这个原生 session id。
    Resume(&'a str),
    /// resume 起不来(stale id):清掉刚才那个残壳 + 坏记录,再冷启。
    KillStaleThenCold,
    /// 没有可续接的记录(或没开 resume):直接冷启。
    Cold,
    /// 跨群 fork 已落盘、副本还没生出来:按 fork 形态启动 —— 从 `source_sid` 取历史,
    /// 落到记录里的 `new_sid` 上。
    ///
    /// **必须先于 resume 判**:记录里的 sid 此刻在 provider 侧还不存在,拿它去 resume 必然失败,
    /// 于是落进 `KillStaleThenCold` 把整条 fork 记录清掉 —— fork 静默降级成冷启,目标群拿到
    /// 一个空会话 —— 而目标群的人刚发过那句 `/fork` 接受它。
    ForkFrom {
        source_sid: &'a str,
        new_sid: &'a str,
    },
}

/// 判定本次该复用 / 续接 / 清残壳后冷启 / 直接冷启。
///
/// - `has_session`:tmux 会话此刻是否活着(唯一无条件探测)。
/// - `resume_enabled`:driver 支持 resume **且** agent/全局配置都没关掉。
/// - `stored_id`:盘上记录的 CLI 原生 session id。
/// - `resume_ok`:已按 driver 语法起过 resume 命令、且探活确认真续起来了。
///   只有 `stored_id` 非空且启用 resume 时调用方才会真去起,其余情况恒为 false。
/// - `fork`:记录带 `fork_pending` 时的 `(源 sid, 该落到的新 sid)`。**两者都从那条记录直接取**,
///   不复用 `stored_id` —— 后者被 `resume_enabled` 与 cli kind guard 缩窄过,拿它当 fork 目标会让
///   「resume 关掉」或「换过 cli」把一次已授权的 fork 静默变成冷启。
///
/// 优先级:活着 > 待 fork > 有记录且续得起来 > 有记录但续不起来 > 冷启。`has_session` 压过一切
/// ——会话活着时调用方根本不会去起 resume 命令,后几个入参此时无意义。
///
/// **待 fork 排在 resume 之前**,理由见 [`StartPlan::ForkFrom`]:记录里那个 sid 在 provider 侧
/// 还不存在,先试 resume 只会把 fork 记录当 stale 清掉。
pub fn plan_start<'a>(
    has_session: bool,
    resume_enabled: bool,
    stored_id: Option<&'a str>,
    resume_ok: bool,
    fork: Option<(&'a str, &'a str)>,
) -> StartPlan<'a> {
    if has_session {
        return StartPlan::AlreadyRunning;
    }
    // fork 自带源 sid 与目标 sid,与 resume 那套入参无关 —— 见 `fork` 参数的说明。
    if let Some((source_sid, new_sid)) = fork {
        return StartPlan::ForkFrom {
            source_sid,
            new_sid,
        };
    }
    match stored_id {
        Some(id) if resume_enabled => {
            if resume_ok {
                StartPlan::Resume(id)
            } else {
                StartPlan::KillStaleThenCold
            }
        }
        _ => StartPlan::Cold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四个 bool/Option 入参的全部 16 种组合(stored_id 只取「有/无」两态)。
    fn all_inputs() -> Vec<(bool, bool, Option<&'static str>, bool)> {
        let mut out = Vec::new();
        for has_session in [false, true] {
            for resume_enabled in [false, true] {
                for stored_id in [None, Some("sid-1")] {
                    for resume_ok in [false, true] {
                        out.push((has_session, resume_enabled, stored_id, resume_ok));
                    }
                }
            }
        }
        out
    }

    /// 全量真值表:一次钉死 16 种组合的结局,任何分支取反都会红。
    #[test]
    fn plan_matches_full_truth_table() {
        let expected = |has_session: bool,
                        resume_enabled: bool,
                        stored_id: Option<&'static str>,
                        resume_ok: bool| {
            match (has_session, resume_enabled, stored_id, resume_ok) {
                (true, _, _, _) => StartPlan::AlreadyRunning,
                (false, true, Some(id), true) => StartPlan::Resume(id),
                (false, true, Some(_), false) => StartPlan::KillStaleThenCold,
                _ => StartPlan::Cold,
            }
        };
        for (has_session, resume_enabled, stored_id, resume_ok) in all_inputs() {
            assert_eq!(
                plan_start(has_session, resume_enabled, stored_id, resume_ok, None),
                expected(has_session, resume_enabled, stored_id, resume_ok),
                "组合 session={has_session} enabled={resume_enabled} id={stored_id:?} ok={resume_ok}"
            );
        }
    }

    #[test]
    fn a_pending_fork_starts_from_the_source_instead_of_resuming_the_unborn_sid() {
        assert_eq!(
            plan_start(
                false,
                true,
                Some("new-sid"),
                false,
                Some(("src-sid", "new-sid"))
            ),
            StartPlan::ForkFrom {
                source_sid: "src-sid",
                new_sid: "new-sid",
            },
            "待 fork 必须从源取历史,而不是去 resume 一个还不存在的 sid"
        );
    }

    /// 这条是 fork 的命门。记录里那个 sid 在 provider 侧还不存在,所以 resume 探活**必然**失败
    /// (`resume_ok=false`)—— 若 fork 判据排在 resume 之后,每次都会落进 `KillStaleThenCold`,
    /// 把刚提交的那条 fork 记录直接清掉,目标群拿到一个空会话。
    #[test]
    fn a_pending_fork_is_never_mistaken_for_a_stale_record() {
        for resume_ok in [false, true] {
            for resume_enabled in [false, true] {
                let plan = plan_start(
                    false,
                    resume_enabled,
                    Some("new-sid"),
                    resume_ok,
                    Some(("src-sid", "new-sid")),
                );
                assert!(
                    matches!(plan, StartPlan::ForkFrom { .. }),
                    "待 fork 不该被当成 stale 或冷启(enabled={resume_enabled} ok={resume_ok}),实际 {plan:?}"
                );
            }
        }
    }

    #[test]
    fn a_live_session_still_wins_over_a_pending_fork() {
        // tmux 活着说明这一轮的上下文已经在内存里了。此时按 fork 重起会把它顶掉,
        // 而 fork 的副本本来就该由**第一轮**生成 —— 会话活着意味着那一轮已经开始或跑完了。
        assert_eq!(
            plan_start(
                true,
                true,
                Some("new-sid"),
                false,
                Some(("src-sid", "new-sid"))
            ),
            StartPlan::AlreadyRunning
        );
    }

    /// fork 的执行**不依赖 resume 那套入参**。
    ///
    /// `stored_id` 被两道过滤缩窄过:`resume_sessions: false` 时调用方压根不去读它,换过 cli 时
    /// kind guard 会把它滤掉。如果 fork 的目标 sid 取自它,这两种情形下 fork 记录明明完好却会
    /// 落进冷启 —— 而两个群刚各发过一条 `/fork`、目标群已经收到「已接上上下文副本」的回执。
    /// 空会话和带着上下文的会话,用户分不出来。
    #[test]
    fn a_pending_fork_runs_even_when_resume_is_off_or_the_stored_id_was_filtered_out() {
        for resume_enabled in [false, true] {
            assert_eq!(
                plan_start(
                    false,
                    resume_enabled,
                    // resume 关掉 / kind guard 滤掉 → 调用方给不出 stored_id。
                    None,
                    false,
                    Some(("src-sid", "new-sid"))
                ),
                StartPlan::ForkFrom {
                    source_sid: "src-sid",
                    new_sid: "new-sid",
                },
                "fork 是显式命令,不该被 resume 开关或 cli 变更悄悄取消(enabled={resume_enabled})"
            );
        }
    }
}
