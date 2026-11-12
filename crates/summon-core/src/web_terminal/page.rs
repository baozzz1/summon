//! 终端服务的三张页面。页面里只放展示用的名字,码与密钥从不写进 HTML。

use super::grants::TerminalMode;

const TERMINAL_TEMPLATE: &str = include_str!("terminal.html");

const STYLE: &str = "body{margin:0;padding:24px;background:#1e1e1e;color:#ddd;\
font:15px -apple-system,BlinkMacSystemFont,sans-serif}button{padding:10px 24px;font-size:16px}";

fn mode_label(mode: TerminalMode) -> &'static str {
    match mode {
        TerminalMode::ReadOnly => summon_i18n::text!("ree4f6f4d54"),
        TerminalMode::Operate => summon_i18n::text!("r5adc58e049"),
    }
}

pub fn landing_page(mode: TerminalMode, code: &str) -> String {
    summon_i18n::format!("r05ab13a5d2", label = mode_label(mode),)
}

/// 码还有效,但收件人还没在飞书里点卡片按钮(或点过已超过 1 分钟)。
pub fn unconfirmed_page() -> String {
    summon_i18n::format!("re6c8785879")
}

pub fn invalid_page() -> String {
    summon_i18n::format!("rda1e8fd78e")
}

pub fn terminal_page(handle: &str, mode: TerminalMode, agent: &str, chat_label: &str) -> String {
    let operate = mode == TerminalMode::Operate;
    let hint = if operate {
        summon_i18n::text!("re31842243a")
    } else {
        ""
    };
    let controls = if operate {
        summon_i18n::text!("r3ea6a7f37f")
    } else {
        ""
    };
    // 名字里的花括号已被 escape_html 转成实体,名字里写 `{{handle}}` 也不会被当成占位符。
    TERMINAL_TEMPLATE
        .replace("{{bar_class}}", if operate { "op" } else { "" })
        .replace("{{mode_label}}", mode_label(mode))
        .replace("{{hint}}", hint)
        .replace("{{controls}}", controls)
        .replace("{{mode}}", mode.as_str())
        .replace("{{handle}}", handle)
        .replace("{{agent}}", &escape_html(agent))
        .replace("{{chat}}", &escape_html(chat_label))
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('{', "&#123;")
        .replace('}', "&#125;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_landing_page_posts_to_enter_and_names_only_the_mode() {
        let html = landing_page(TerminalMode::Operate, "abc");
        assert!(html.contains(r#"method="post""#));
        assert!(html.contains(r#"action="/t/abc/enter""#));
        assert!(html.contains("可操作"));
    }

    #[test]
    fn the_terminal_page_escapes_names_and_carries_mode_and_socket_path() {
        let html = terminal_page("h1", TerminalMode::ReadOnly, "<b>a</b>", "群&名");
        assert!(html.contains("&lt;b&gt;a&lt;/b&gt;"));
        assert!(html.contains("群&amp;名"));
        assert!(html.contains(r#"const MODE = "read";"#));
        assert!(html.contains("/t/s/h1/ws"));
        assert!(html.contains("/t/assets/xterm.js"));
        assert!(!html.contains("{{"), "模板占位符必须全部替换");
        assert!(!html.contains(r#"id="keys""#), "只读页没有快捷键栏");
        let op = terminal_page("h1", TerminalMode::Operate, "a", "b");
        assert!(op.contains(r#"id="keys""#));
        assert!(op.contains("summon 不会停下来等你"));
    }

    /// 行为由 summon 那边的 ignored 测试用真 xterm.js 验;这里钉住接线:连上之前就装好。
    #[test]
    fn the_page_terminal_is_quieted_before_it_connects() {
        for mode in [TerminalMode::ReadOnly, TerminalMode::Operate] {
            let html = terminal_page("h1", mode, "a", "b");
            let quiet = html.find("\nquiet(term);\n").expect("装上 quiet");
            let connect = html.rfind("\nconnect();\n").expect("连接");
            assert!(quiet < connect);
        }
    }

    /// 两种页面都把滚轮与上下滑换成 `{"scroll": n}`(文本帧),滚轮一律不交给 xterm.js;按键只有可操作页
    /// 发,走二进制帧。页面里往 WebSocket 发东西的只有这两处。换算本身由 summon 那边的 ignored
    /// 测试用 node 跑页面里的 `scroller` 验。
    #[test]
    fn both_pages_send_scroll_messages_and_only_the_operate_page_sends_keystrokes_as_binary() {
        for mode in [TerminalMode::ReadOnly, TerminalMode::Operate] {
            let html = terminal_page("h1", mode, "a", "b");
            assert!(html.contains("function scroller(post, rowPx, rows, now, later, canScroll) {"));
            assert!(html.contains("ws.send(JSON.stringify({scroll: n}))"));
            let wheel = html
                .find("term.attachCustomWheelEventHandler((e) => {")
                .expect("滚轮由页面自己处理");
            let rest = &html[wheel..];
            let body = &rest[..rest.find("\n});\n").expect("滚轮处理的结尾")];
            assert!(
                body.contains("if (scroll.wheel(e)) e.preventDefault();"),
                "归滚动的才拦:{body}"
            );
            assert!(
                body.ends_with("\n  return false;"),
                "不让 xterm.js 把滚轮折成方向键:{body}"
            );
            assert!(html.contains(r#"area.addEventListener("touchmove", "#));
            assert!(html.contains(
                r#"function send(s) { if (MODE === "operate" && live()) ws.send(encoder.encode(s)); }"#
            ));
            assert_eq!(html.matches("ws.send(").count(), 2, "只有按键与滚动两处");
            assert!(!html.contains("{{"), "模板占位符必须全部替换");
        }
    }

    #[test]
    fn names_that_look_like_placeholders_are_shown_literally() {
        // 测试 agent 名与 chat 名互换时不会相互覆盖
        // agent={{chat}}, chat=REALCHAT 的情况
        let html1 = terminal_page("h1", TerminalMode::Operate, "{{chat}}", "REALCHAT");
        // agent 名中的 {{chat}} 被转义为 &#123;&#123;chat&#125;&#125;
        assert!(
            html1.contains("&#123;&#123;chat&#125;&#125;"),
            "agent 名 {{{{chat}}}} 应该被转义显示"
        );
        // REALCHAT 应该恰好出现一次(只在 #bar 中显示为 chat 标签)
        assert_eq!(
            html1.matches("REALCHAT").count(),
            1,
            "REALCHAT 应该恰好出现一次"
        );
        assert_eq!(
            html1.matches(r#"id="keys""#).count(),
            1,
            "#keys 应该恰好出现一次"
        );

        // chat={{agent}}, agent=REALAGENT 的情况
        let html2 = terminal_page("h1", TerminalMode::Operate, "REALAGENT", "{{agent}}");
        // chat 名中的 {{agent}} 被转义为 &#123;&#123;agent&#125;&#125;
        assert!(
            html2.contains("&#123;&#123;agent&#125;&#125;"),
            "chat 名 {{{{agent}}}} 应该被转义显示"
        );
        // REALAGENT 应该恰好出现一次(只在 #bar 中显示为 agent 标签)
        assert_eq!(
            html2.matches("REALAGENT").count(),
            1,
            "REALAGENT 应该恰好出现一次"
        );
        assert_eq!(
            html2.matches(r#"id="keys""#).count(),
            1,
            "#keys 应该恰好出现一次"
        );

        // 同时测试固定占位符文本的情况
        let html3 = terminal_page("h1", TerminalMode::Operate, "{{mode}}", "{{controls}}");
        assert!(
            html3.contains("&#123;&#123;mode&#125;&#125;"),
            "agent 名 {{{{mode}}}} 应该被转义显示"
        );
        assert!(
            html3.contains("&#123;&#123;controls&#125;&#125;"),
            "chat 名 {{{{controls}}}} 应该被转义显示"
        );
        assert_eq!(
            html3.matches(r#"id="keys""#).count(),
            1,
            "#keys 应该恰好出现一次"
        );
        assert_eq!(
            html3.matches(r#"id="line""#).count(),
            1,
            "#line 应该恰好出现一次"
        );
    }
}
