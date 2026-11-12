//! summon 原生 macOS Menu Bar 的 launchd 纯配置 + 「在终端里打开会话」的 argv 渲染。

use std::path::Path;

pub const LAUNCHD_LABEL: &str = "com.summon.menubar";

pub fn plist_file_name() -> String {
    format!("{LAUNCHD_LABEL}.plist")
}

/// Menu Bar 是登录用户的原生 GUI 进程：launchd 负责登录即起和异常保活，所有业务动作
/// 仍经 summon CLI 收口，不开放 HTTP，也不让 Swift 进程直接解释持久化目录。
/// **`KeepAlive` 用 `SuccessfulExit: false` 而不是裸 `true`。**
///
/// 裸 `true` = 无条件复活:菜单里那个「退出 Menu Bar」按下去,进程确实退了,launchd 立刻又把它
/// 拉起来 —— 用户看到的是"退不掉"(owner 2026-08-10 实测)。**给了退出入口却让它无效,比不给
/// 更糟**:人会以为程序卡死。
///
/// 换成 `SuccessfulExit: false` 后两种情形各归各位:正常退出(exit 0,即用户点退出)**不复活**;
/// 异常退出(崩溃/被 kill)仍然自愈 —— 后者才是 KeepAlive 本来要解决的问题。
pub fn render_launchd_plist(binary: &Path, summon_cli: &Path, log: &Path) -> String {
    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    let binary = xml_escape(&binary.to_string_lossy());
    let summon_cli = xml_escape(&summon_cli.to_string_lossy());
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>SUMMON_CLI</key>
        <string>{summon_cli}</string>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// 把「在终端里跑 `cmd`」渲染成一条可执行 argv。
///
/// `template` 来自 `config.yaml` 的 `menubar.terminal`,占位符:
/// - `{cmd}`   要跑的 summon 命令(如 `summon agents attach soul-x-g1a2b3c4`)
/// - `{title}` 会话标题(终端窗口/标签名;模板不含它也无妨)
///
/// 空模板 = 没配 → 回落系统 Terminal.app(osascript)。为什么用 osascript 而不是 `open -a`:
/// `open -a Terminal` 只能开一个新窗口跑**文件**,要跑一条命令得先落临时脚本文件——
/// 平白引入一个要清理的盘上产物。osascript 直接把命令送进 Terminal,无副产物。
pub fn render_terminal_argv(template: &[String], cmd: &str, title: &str) -> Vec<String> {
    if template.is_empty() {
        let script = format!(
            r#"tell application "Terminal" to do script "{}""#,
            applescript_escape(cmd)
        );
        return vec![
            "osascript".into(),
            "-e".into(),
            script,
            "-e".into(),
            r#"tell application "Terminal" to activate"#.into(),
        ];
    }
    template
        .iter()
        .map(|arg| arg.replace("{cmd}", cmd).replace("{title}", title))
        .collect()
}

/// Menu Bar 状态栏可选的 summon 图标。**顺序就是 onboard 向导里的编号**(1 起,第一个是默认);
/// config 存 `name` 而不是编号 —— 以后增删候选,已有的选择不会指向别的图标。
/// 几何的唯一来源是 `menubar/Icons/summon-<name>.svg`。
pub struct MenubarIcon {
    pub name: &'static str,
    pub label: &'static str,
    pub meaning: &'static str,
}

pub const MENUBAR_ICONS: &[MenubarIcon] = &[
    MenubarIcon {
        name: "xyz",
        label: "r357c359a24",
        meaning: "rfc37ea4a99",
    },
    MenubarIcon {
        name: "millennium-eye",
        label: "rea3e1df9ff",
        meaning: "rd9136bd854",
    },
    MenubarIcon {
        name: "link",
        label: "r91afe57d54",
        meaning: "r01b474e9c3",
    },
    MenubarIcon {
        name: "synchro",
        label: "r4921c33aea",
        meaning: "ra2ed85230b",
    },
    MenubarIcon {
        name: "card",
        label: "r97e13b5d2d",
        meaning: "r7b308f0609",
    },
    MenubarIcon {
        name: "fusion",
        label: "r0d0066249e",
        meaning: "r85cfaaf439",
    },
    MenubarIcon {
        name: "ring-star",
        label: "r1297a4b570",
        meaning: "r8478b7fcb0",
    },
    MenubarIcon {
        name: "hex-star",
        label: "r908bb5862b",
        meaning: "r155e53bc08",
    },
];

pub const DEFAULT_MENUBAR_ICON: &str = "xyz";

/// 生效的图标名。认不出的名字(手改错、旧版本删掉的候选)按默认画,不让状态栏空掉。
pub fn resolve_menubar_icon(configured: Option<&str>) -> &'static str {
    configured
        .map(str::trim)
        .and_then(|name| MENUBAR_ICONS.iter().find(|icon| icon.name == name))
        .map(|icon| icon.name)
        .unwrap_or(DEFAULT_MENUBAR_ICON)
}

/// 向导输入 → 图标。回车取第一个;编号从 1 起。
pub fn menubar_icon_for_choice(input: &str) -> Option<&'static MenubarIcon> {
    let input = input.trim();
    if input.is_empty() {
        return MENUBAR_ICONS.first();
    }
    let index: usize = input.parse().ok()?;
    MENUBAR_ICONS.get(index.checked_sub(1)?)
}

/// 在 config.yaml 原文里写 `menubar.icon`,其余内容(含注释)原样保留。按行改而不是整份
/// 反序列化再写回 —— 后者会丢掉用户的注释与排版。inline 写法的 `menubar:` 改不安全,直接拒绝。
pub fn patch_menubar_icon_yaml(raw: &str, name: &str) -> Result<String, String> {
    let is_top_level = |line: &str| {
        !line.starts_with([' ', '\t'])
            && !line.trim().is_empty()
            && !line.trim_start().starts_with('#')
    };
    // 节标题后面可以跟注释(`menubar:  # 状态栏`),去掉注释再认。
    let is_section_header = |line: &str| {
        let code = line.split_once('#').map_or(line, |(code, _)| code);
        is_top_level(line) && code.trim_end() == "menubar:"
    };
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    match lines.iter().position(|line| is_section_header(line)) {
        Some(start) => {
            let end = lines
                .iter()
                .enumerate()
                .skip(start + 1)
                .find(|(_, line)| is_top_level(line))
                .map(|(index, _)| index)
                .unwrap_or(lines.len());
            let indent = lines[start + 1..end]
                .iter()
                .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                .map(|line| line[..line.len() - line.trim_start().len()].to_string())
                .unwrap_or_else(|| "  ".to_string());
            let entry = format!("{indent}icon: {name}");
            let key = format!("{indent}icon:");
            match (start + 1..end).find(|&index| lines[index].starts_with(&key)) {
                Some(index) => lines[index] = entry,
                None => lines.insert(start + 1, entry),
            }
        }
        None => {
            if lines
                .iter()
                .any(|line| is_top_level(line) && line.starts_with("menubar:"))
            {
                return Err(summon_i18n::text!("rca46f301b9").to_string());
            }
            if lines.last().is_some_and(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            lines.push("menubar:".to_string());
            lines.push(format!("  icon: {name}"));
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

/// AppleScript 字符串字面量转义:反斜杠必须先转,否则会把后续转义出来的引号再转一遍。
fn applescript_escape(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_terminal_drives_terminal_app_via_osascript() {
        let argv = render_terminal_argv(&[], "summon agents attach soul-x-g1a2b3c4", "研发群");
        assert_eq!(argv[0], "osascript");
        assert!(argv[2].contains(r#"do script "summon agents attach soul-x-g1a2b3c4""#));
        assert!(argv[4].contains("activate"));
    }

    #[test]
    fn configured_template_substitutes_cmd_and_title() {
        let template: Vec<String> = [
            "cmux",
            "new-workspace",
            "--name",
            "{title}",
            "--command",
            "{cmd}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let argv = render_terminal_argv(&template, "summon agents tail soul-x-p9f8", "私聊 · 老板");
        assert_eq!(
            argv,
            vec![
                "cmux",
                "new-workspace",
                "--name",
                "私聊 · 老板",
                "--command",
                "summon agents tail soul-x-p9f8",
            ]
        );
    }

    #[test]
    fn applescript_escapes_quotes_and_backslashes() {
        // 会话名理论上不含引号,但模板走的是用户输入面 —— 转义漏了就是命令注入。
        let argv = render_terminal_argv(&[], r#"echo "hi" \ there"#, "t");
        assert!(argv[2].contains(r#"echo \"hi\" \\ there"#));
    }

    #[test]
    fn template_without_title_placeholder_still_works() {
        let template: Vec<String> = ["kitty", "--", "sh", "-c", "{cmd}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let argv = render_terminal_argv(&template, "summon agents attach s", "群");
        assert_eq!(
            argv,
            vec!["kitty", "--", "sh", "-c", "summon agents attach s"]
        );
    }

    #[test]
    fn plist_runs_native_menubar_with_summon_cli() {
        let plist = render_launchd_plist(
            Path::new("/Users/me/.summon/bin/summon-menubar"),
            Path::new("/Users/me/.local/bin/summon"),
            Path::new("/Users/me/.summon/run/menubar.log"),
        );
        assert!(plist.contains("<string>com.summon.menubar</string>"));
        assert!(plist.contains("<string>/Users/me/.summon/bin/summon-menubar</string>"));
        assert!(plist.contains("<key>SUMMON_CLI</key>"));
        assert!(plist.contains("<string>/Users/me/.local/bin/summon</string>"));
        assert!(plist.contains("<key>ProcessType</key>\n    <string>Interactive</string>"));
        // KeepAlive 的**语义**由 `quit_semantics_tests` 钉:按退出码区分,正常退出不复活。
        // 这里只确认它还在(崩溃自愈没被一起删掉)。
        assert!(plist.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn plist_escapes_all_user_paths() {
        let plist = render_launchd_plist(
            Path::new("/Users/a&b/menu<bar>"),
            Path::new("/Users/a&b/summon"),
            Path::new("/Users/a&b/menu.log"),
        );
        assert!(plist.contains("/Users/a&amp;b/menu&lt;bar&gt;"));
        assert!(plist.contains("/Users/a&amp;b/summon"));
        assert!(plist.contains("/Users/a&amp;b/menu.log"));
    }
}

#[cfg(test)]
mod quit_semantics_tests {
    use super::*;
    use std::path::Path;

    /// **菜单里的「退出 Menu Bar」必须真能退出**(owner 2026-08-10 实测:点了之后又被拉起来)。
    ///
    /// 裸 `KeepAlive: true` 会无条件复活进程,把一个存在的退出入口变成假的。改用
    /// `SuccessfulExit: false`:正常退出不复活,崩溃仍自愈。
    #[test]
    fn keep_alive_does_not_resurrect_a_clean_quit() {
        let plist = render_launchd_plist(
            Path::new("/bin/true"),
            Path::new("/usr/local/bin/summon"),
            Path::new("/tmp/menubar.log"),
        );
        assert!(
            plist.contains("<key>SuccessfulExit</key>"),
            "KeepAlive 必须按退出码区分,否则用户点退出也会被拉起:{plist}"
        );
        // 裸 <key>KeepAlive</key> 紧跟 <true/> 就是无条件复活 —— 禁词运行时拼接,免得自扫。
        let unconditional = format!("<key>KeepAlive</key>\n    <{}/>", "true");
        assert!(!plist.contains(&unconditional), "不许退回无条件 KeepAlive");
        // 崩溃自愈这半边不能一起丢掉。
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
    }
}

#[cfg(test)]
mod icon_tests {
    use super::*;

    #[test]
    fn default_icon_is_first_and_unknown_names_fall_back_to_it() {
        assert_eq!(MENUBAR_ICONS[0].name, DEFAULT_MENUBAR_ICON);
        assert_eq!(resolve_menubar_icon(None), "xyz");
        assert_eq!(
            resolve_menubar_icon(Some("millennium-eye")),
            "millennium-eye"
        );
        assert_eq!(resolve_menubar_icon(Some(" link ")), "link");
        assert_eq!(
            resolve_menubar_icon(Some("no-such-icon")),
            "xyz",
            "认不出的名字按默认画"
        );
    }

    #[test]
    fn wizard_choice_is_one_based_and_empty_means_default() {
        assert_eq!(menubar_icon_for_choice("").map(|i| i.name), Some("xyz"));
        assert_eq!(
            menubar_icon_for_choice(" 2 ").map(|i| i.name),
            Some("millennium-eye")
        );
        assert_eq!(
            menubar_icon_for_choice(&MENUBAR_ICONS.len().to_string()).map(|i| i.name),
            Some("hex-star")
        );
        assert!(menubar_icon_for_choice("0").is_none());
        assert!(menubar_icon_for_choice("9").is_none());
        assert!(menubar_icon_for_choice("xyz").is_none());
    }

    #[test]
    fn icon_names_match_the_svg_sources() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../menubar/Icons");
        for icon in MENUBAR_ICONS {
            let path = format!("{dir}/summon-{}.svg", icon.name);
            assert!(std::path::Path::new(&path).exists(), "缺 {path}");
        }
    }

    #[test]
    fn patch_appends_section_when_absent() {
        let out = patch_menubar_icon_yaml("default_domain: feishu.cn\n", "link").unwrap();
        assert_eq!(out, "default_domain: feishu.cn\n\nmenubar:\n  icon: link\n");
    }

    #[test]
    fn patch_inserts_into_existing_section_with_its_indent() {
        let raw = "menubar:\n    terminal: [\"cmux\"]\nsentinel:\n  enabled: false\n";
        let out = patch_menubar_icon_yaml(raw, "card").unwrap();
        assert_eq!(
            out,
            "menubar:\n    icon: card\n    terminal: [\"cmux\"]\nsentinel:\n  enabled: false\n"
        );
        let cfg: crate::config::Config = serde_yaml::from_str(&out).unwrap();
        assert_eq!(cfg.menubar.icon.as_deref(), Some("card"));
        assert_eq!(cfg.menubar.terminal, vec!["cmux".to_string()]);
    }

    #[test]
    fn patch_replaces_existing_icon() {
        let out = patch_menubar_icon_yaml("menubar:\n  icon: xyz\n", "fusion").unwrap();
        assert_eq!(out, "menubar:\n  icon: fusion\n");
    }

    #[test]
    fn patch_keeps_a_trailing_comment_on_the_section_header() {
        let out = patch_menubar_icon_yaml("menubar:  # 状态栏\n  terminal: []\n", "link").unwrap();
        assert_eq!(out, "menubar:  # 状态栏\n  icon: link\n  terminal: []\n");
    }

    #[test]
    fn patch_refuses_inline_mapping() {
        assert!(patch_menubar_icon_yaml("menubar: {terminal: []}\n", "xyz").is_err());
    }
}
