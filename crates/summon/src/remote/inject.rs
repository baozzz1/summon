use std::time::Duration;

use summon_core::parse::remote_screen::{
    input_box_holds, input_box_text, refusal_notice, remote_gate,
};

use super::cmux::{is_auth_failure, CmuxPort};
use crate::observe::log::log;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeliverOutcome {
    Submitted,
    Refused(String),
}

const MISMATCH: &str = "r0b2696c9fa";

/// 出错的是哪一类 cmux 命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    ReadScreen,
    Input,
}

/// 群里怎么说 cmux 出错的原因:按出错的命令区分,只有确认是鉴权失败才提示密码模式。错误链
/// (stderr、路径)只进日志。
fn cause(step: Step, error: &anyhow::Error) -> String {
    let base = match step {
        Step::ReadScreen => summon_i18n::text!("rca75a96bbd"),
        Step::Input => summon_i18n::text!("rd0b4dfd7a0"),
    };
    if is_auth_failure(error) {
        summon_i18n::format!("r8bbb69c5f0")
    } else {
        base.to_string()
    }
}

// 下面四条通知按 cmux 报错时消息已走到哪一步来写:用户据此决定要不要重发,
// 说错阶段会造成漏发或重复投递。
fn not_sent_notice(cause: &str) -> String {
    summon_i18n::format!("rf21aef93d1")
}

fn typed_not_submitted_notice(cause: &str) -> String {
    summon_i18n::format!("rcc7a45da78")
}

fn enter_failed_notice(cause: &str) -> String {
    summon_i18n::format!("r3d5f49c293")
}

fn submit_unreadable_notice(cause: &str) -> String {
    summon_i18n::format!("r55ef7dbc9f")
}

/// 逐行打字(行间 shift+enter 换行),等输入框稳定后读屏。出错时带上是哪一类命令。
fn type_message(
    port: &dyn CmuxPort,
    surface: &str,
    lines: &[String],
    settle: Duration,
) -> Result<String, (Step, anyhow::Error)> {
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            port.send_key(surface, "shift+enter")
                .map_err(|error| (Step::Input, error))?;
        }
        if !line.is_empty() {
            port.send_text(surface, line)
                .map_err(|error| (Step::Input, error))?;
        }
    }
    std::thread::sleep(settle);
    port.read_screen(surface)
        .map_err(|error| (Step::ReadScreen, error))
}

/// 把一条群消息打进 cmux 窗口并提交。按回车之前必须证明输入框里恰好是这条消息;
/// 证明不了就停在原地,输入框里的字一个都不删。回车之后只有读到空输入框才算已提交。
/// `log_tag` 标明是哪个 agent、哪个群,cmux 的完整错误只写日志。
pub(crate) fn deliver(
    port: &dyn CmuxPort,
    surface: &str,
    lines: &[String],
    settle: Duration,
    log_tag: &str,
) -> DeliverOutcome {
    let failed = |phase: &str, step: Step, error: &anyhow::Error| {
        log(&summon_i18n::format!("rd24c3bdde8"));
        cause(step, error)
    };
    let before = match port.read_screen(surface) {
        Ok(screen) => screen,
        Err(error) => {
            let cause = failed(summon_i18n::text!("rb882ae40be"), Step::ReadScreen, &error);
            return DeliverOutcome::Refused(not_sent_notice(&cause));
        }
    };
    if let Err(refusal) = remote_gate(&before) {
        return DeliverOutcome::Refused(refusal_notice(refusal).to_string());
    }
    let typed = match type_message(port, surface, lines, settle) {
        Ok(screen) => screen,
        Err((step, error)) => {
            let cause = failed(summon_i18n::text!("rd1ba8fcbed"), step, &error);
            return DeliverOutcome::Refused(typed_not_submitted_notice(&cause));
        }
    };
    if !input_box_holds(&typed, &lines.join("\n")) {
        return DeliverOutcome::Refused(::summon_core::i18n::label(MISMATCH).into());
    }
    if let Err(error) = port.send_key(surface, "enter") {
        let cause = failed(summon_i18n::text!("r03c528e623"), Step::Input, &error);
        return DeliverOutcome::Refused(enter_failed_notice(&cause));
    }
    std::thread::sleep(settle);
    let after = match port.read_screen(surface) {
        Ok(screen) => screen,
        Err(error) => {
            let cause = failed(summon_i18n::text!("re0e41f49a8"), Step::ReadScreen, &error);
            return DeliverOutcome::Refused(submit_unreadable_notice(&cause));
        }
    };
    // 回车后输入框必须读得出且为空;读不出、仍是本条消息或变成别的字都不能算已提交。
    if input_box_text(&after).as_deref() == Some("") {
        DeliverOutcome::Submitted
    } else {
        DeliverOutcome::Refused(summon_i18n::text!("ree0d88bb2b").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 按调用顺序依次返回预置屏幕;记录所有发出的按键与文字。
    /// 失败注入:第 N 次读屏、`send_text`、指定按键各自可以单独报错,报错文字可换。
    struct Fake {
        screens: Mutex<Vec<String>>,
        sent: Mutex<Vec<String>>,
        reads: Mutex<usize>,
        fail_read_at: Option<usize>,
        fail_text: bool,
        fail_key: Option<&'static str>,
        error: &'static str,
    }

    const AUTH_ERROR: &str = "cmux read-screen 失败:ERROR: Access denied";
    const TIMEOUT_ERROR: &str = "cmux read-screen 超时";

    impl Fake {
        fn new(screens: &[&str]) -> Self {
            let mut s: Vec<String> = screens.iter().map(|s| s.to_string()).collect();
            s.reverse();
            Self {
                screens: Mutex::new(s),
                sent: Mutex::new(Vec::new()),
                reads: Mutex::new(0),
                fail_read_at: None,
                fail_text: false,
                fail_key: None,
                error: AUTH_ERROR,
            }
        }
        fn failing_read_at(mut self, index: usize) -> Self {
            self.fail_read_at = Some(index);
            self
        }
        fn failing_text(mut self) -> Self {
            self.fail_text = true;
            self
        }
        fn failing_key(mut self, key: &'static str) -> Self {
            self.fail_key = Some(key);
            self
        }
        fn with_error(mut self, error: &'static str) -> Self {
            self.error = error;
            self
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
        fn pressed_enter(&self) -> bool {
            self.sent().contains(&"key:enter".to_string())
        }
    }

    impl CmuxPort for Fake {
        fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn read_screen(&self, _: &str) -> anyhow::Result<String> {
            let mut reads = self.reads.lock().unwrap();
            let index = *reads;
            *reads += 1;
            if self.fail_read_at == Some(index) {
                anyhow::bail!(self.error)
            }
            Ok(self
                .screens
                .lock()
                .unwrap()
                .pop()
                .expect("读屏次数超出预期"))
        }
        fn send_text(&self, _: &str, text: &str) -> anyhow::Result<()> {
            if self.fail_text {
                anyhow::bail!(self.error)
            }
            self.sent.lock().unwrap().push(format!("text:{text}"));
            Ok(())
        }
        fn send_key(&self, _: &str, key: &str) -> anyhow::Result<()> {
            if self.fail_key == Some(key) {
                anyhow::bail!(self.error)
            }
            self.sent.lock().unwrap().push(format!("key:{key}"));
            Ok(())
        }
    }

    const RULE: &str = "────────────────────────────────────────";

    fn screen(input: &[&str]) -> String {
        let mut lines = vec!["⏺ 好的".to_string(), RULE.to_string()];
        for (i, l) in input.iter().enumerate() {
            lines.push(if i == 0 {
                format!("❯ {l}")
            } else {
                format!("  {l}")
            });
        }
        lines.push(RULE.to_string());
        lines.push("  ⏵⏵ auto mode on".to_string());
        lines.join("\n")
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn deliver_to(fake: &Fake, message: &[&str]) -> DeliverOutcome {
        deliver(fake, "S", &lines(message), Duration::ZERO, "[t] chat=oc_t")
    }

    fn refused(out: &DeliverOutcome) -> &str {
        match out {
            DeliverOutcome::Refused(notice) => notice,
            DeliverOutcome::Submitted => panic!("预期被拒,实际 Submitted"),
        }
    }

    /// 群里的说明只用固定短语:cmux 的错误原文(stderr、路径)只进日志。
    fn assert_no_error_text(notice: &str) {
        for raw in ["Access denied", "ERROR", "超时", "read-screen"] {
            assert!(!notice.contains(raw), "群里不带错误原文 {raw}:{notice}");
        }
    }

    #[test]
    fn submits_after_proving_the_box_holds_exactly_the_message() {
        let fake = Fake::new(&[
            &screen(&[""]),
            &screen(&["第一行", "第二行"]),
            &screen(&[""]),
        ]);
        let out = deliver_to(&fake, &["第一行", "第二行"]);
        assert_eq!(out, DeliverOutcome::Submitted);
        assert_eq!(
            fake.sent(),
            vec!["text:第一行", "key:shift+enter", "text:第二行", "key:enter"]
        );
    }

    #[test]
    fn concurrent_terminal_typing_is_not_submitted() {
        let fake = Fake::new(&[&screen(&[""]), &screen(&["我在打字你好"])]);
        let out = deliver_to(&fake, &["你好"]);
        assert_eq!(
            refused(&out),
            "输入框里的内容和这条对不上(可能终端在同时打字),没有按回车,已打的字留在输入框里。到终端里处理后再决定要不要重发。"
        );
        assert!(!fake.pressed_enter(), "比对失败绝不按回车");
    }

    #[test]
    fn draft_before_typing_sends_nothing() {
        let fake = Fake::new(&[&screen(&["半句草稿"])]);
        let out = deliver_to(&fake, &["你好"]);
        assert!(matches!(out, DeliverOutcome::Refused(_)));
        assert!(fake.sent().is_empty());
    }

    #[test]
    fn unreadable_screen_before_typing_says_nothing_was_sent() {
        let fake = Fake::new(&[]).failing_read_at(0);
        let out = deliver_to(&fake, &["你好"]);
        let notice = refused(&out);
        assert_eq!(
            notice,
            "读不到 cmux 窗口(cmux 拒绝了访问,检查 cmux 设置里的 socket 密码模式),这条没有送进去,可以重发。"
        );
        assert_no_error_text(notice);
        assert!(fake.sent().is_empty());
    }

    #[test]
    fn only_an_auth_failure_points_at_the_password_mode() {
        let fake = Fake::new(&[]).failing_read_at(0).with_error(TIMEOUT_ERROR);
        let out = deliver_to(&fake, &["你好"]);
        let notice = refused(&out);
        assert_eq!(notice, "读不到 cmux 窗口,这条没有送进去,可以重发。");
        assert!(!notice.contains("密码"), "{notice}");
    }

    #[test]
    fn text_error_mid_typing_says_the_box_may_hold_text() {
        let fake = Fake::new(&[&screen(&[""])])
            .failing_text()
            .with_error("cmux send 失败:surface not found");
        let out = deliver_to(&fake, &["你好"]);
        let notice = refused(&out);
        assert!(notice.starts_with("cmux 拒绝了输入。"), "{notice}");
        assert!(notice.contains("已有部分或全部文字打进终端输入框,但没有提交"));
        assert!(!notice.contains("没有送进去"));
        assert!(!notice.contains("密码") && !notice.contains("surface not found"));
        assert!(!fake.pressed_enter());
    }

    #[test]
    fn newline_key_error_mid_typing_says_the_box_may_hold_text() {
        let fake = Fake::new(&[&screen(&[""])]).failing_key("shift+enter");
        let out = deliver_to(&fake, &["第一行", "第二行"]);
        let notice = refused(&out);
        assert!(
            notice.starts_with("cmux 拒绝了输入(cmux 拒绝了访问"),
            "{notice}"
        );
        assert!(notice.contains("已有部分或全部文字打进终端输入框,但没有提交"));
        assert_no_error_text(notice);
        assert_eq!(fake.sent(), vec!["text:第一行"]);
    }

    #[test]
    fn unreadable_screen_before_enter_says_the_box_may_hold_text() {
        let fake = Fake::new(&[&screen(&[""])])
            .failing_read_at(1)
            .with_error(TIMEOUT_ERROR);
        let out = deliver_to(&fake, &["你好"]);
        let notice = refused(&out);
        assert!(notice.starts_with("读不到 cmux 窗口。"), "{notice}");
        assert!(notice.contains("已有部分或全部文字打进终端输入框,但没有提交"));
        assert_no_error_text(notice);
        assert_eq!(fake.sent(), vec!["text:你好"]);
    }

    #[test]
    fn enter_error_says_it_may_already_be_submitted() {
        let fake = Fake::new(&[&screen(&[""]), &screen(&["你好"])])
            .failing_key("enter")
            .with_error("cmux send-key 超时");
        let out = deliver_to(&fake, &["你好"]);
        let notice = refused(&out);
        assert_eq!(
            notice,
            "按回车时 cmux 拒绝了输入。这条可能已经提交,到终端里看一眼再决定要不要重发。"
        );
        assert!(!notice.contains("没有送进去"));
    }

    #[test]
    fn unreadable_screen_after_enter_warns_against_resending() {
        let fake = Fake::new(&[&screen(&[""]), &screen(&["你好"])])
            .failing_read_at(2)
            .with_error(TIMEOUT_ERROR);
        let out = deliver_to(&fake, &["你好"]);
        let notice = refused(&out);
        assert_eq!(
            notice,
            "已按回车,但读不到 cmux 窗口,确认不了是否提交成功。到终端里看一眼,别直接重发。"
        );
        assert!(fake.pressed_enter());
    }

    #[test]
    fn unconfirmed_submit_is_reported() {
        let fake = Fake::new(&[&screen(&[""]), &screen(&["你好"]), &screen(&["你好"])]);
        let out = deliver_to(&fake, &["你好"]);
        assert!(matches!(out, DeliverOutcome::Refused(ref n) if n.contains("没能确认")));
        assert!(fake.pressed_enter());
    }

    #[test]
    fn unrecognizable_box_after_enter_is_not_confirmed() {
        let fake = Fake::new(&[
            &screen(&[""]),
            &screen(&["你好"]),
            "⏺ 好的\n  ⏵⏵ auto mode on",
        ]);
        let out = deliver_to(&fake, &["你好"]);
        assert!(matches!(out, DeliverOutcome::Refused(ref n) if n.contains("没能确认")));
        assert!(fake.pressed_enter());
    }

    #[test]
    fn other_text_in_box_after_enter_is_not_confirmed() {
        let fake = Fake::new(&[&screen(&[""]), &screen(&["你好"]), &screen(&["别的字"])]);
        let out = deliver_to(&fake, &["你好"]);
        assert!(matches!(out, DeliverOutcome::Refused(ref n) if n.contains("没能确认")));
    }
}
