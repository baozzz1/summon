import Foundation

/// `summon menubar settings --json` 的契约。整机这一层的值;`overriddenBy` 是在自己 agent.yaml 里
/// 覆盖了同一项的 agent —— Menu Bar 不按 agent 展开,只把名字报出来。
struct MenubarSettings: Decodable, Equatable, Sendable {
    struct Layered<Value: Decodable & Equatable & Sendable>: Decodable, Equatable, Sendable {
        let value: Value
        let overriddenBy: [String]

        enum CodingKeys: String, CodingKey {
            case value
            case overriddenBy = "overridden_by"
        }
    }

    /// "off" / "dry-run" / "on"。
    let sentinel: String
    let showCounts: Bool
    let launchAtLogin: Bool
    let cotVisible: Layered<Bool>
    let hideThinking: Layered<Bool>
    let cotToolDetails: Layered<Bool>
    let badgeEffort: Layered<Bool>
    /// 0 = 关。
    let idleRetireSecs: Layered<UInt64>

    enum CodingKeys: String, CodingKey {
        case sentinel
        case showCounts = "show_counts"
        case launchAtLogin = "launch_at_login"
        case cotVisible = "cot_visible"
        case hideThinking = "hide_thinking"
        case cotToolDetails = "cot_tool_details"
        case badgeEffort = "badge_effort"
        case idleRetireSecs = "idle_retire_secs"
    }

    static func decode(_ data: Data) throws -> MenubarSettings {
        try JSONDecoder().decode(MenubarSettings.self, from: data)
    }
}

/// 设置区一行里的一个胶囊。点它就是 `summon menubar set <key> <value>`。
struct SettingChoice: Equatable, Sendable {
    let title: String
    let key: String
    /// 点下去要写的值。多选行的胶囊写的是「翻转后」的值。
    let value: String
    let selected: Bool
    let overriddenBy: [String]
}

/// 设置区的一行:标签 + 一排胶囊。`id` 稳定,菜单开着时按它原地换数据。
struct SettingRow: Equatable, Sendable {
    let id: String
    let label: String
    let choices: [SettingChoice]

    /// 自己覆盖了这一行某项的 agent(去重、按名字排)。
    var overriddenBy: [String] {
        Array(Set(choices.flatMap(\.overriddenBy))).sorted()
    }
}

/// 设置值 → 设置区的行。纯函数:点哪个胶囊写什么、哪个亮,全在这里定。
///
/// 还没取到设置时(`nil`)结构不变、胶囊都不亮:菜单开着时不能增删行,取到后原地换上。
enum SettingsLayout {
    static var autoExitPresets: [(String, UInt64)] { [(L10n.text("mca7981b4"), 0), ("1h", 3_600), ("1d", 86_400)] }

    static func rows(_ s: MenubarSettings?) -> [SettingRow] {
        let sentinel = [(L10n.text("mca7981b4"), "off"), (L10n.text("m896f330b"), "dry-run"), (L10n.text("m13001175"), "on")].map { title, value in
            SettingChoice(
                title: title, key: "sentinel", value: value,
                selected: s?.sentinel == value, overriddenBy: []
            )
        }
        let feishu = [
            toggle(L10n.text("me083bd83"), key: "cot_visible", s?.cotVisible),
            // 配置项是「隐藏思考」,胶囊说的是「显示思考」:亮着 = 显示。
            SettingChoice(
                title: L10n.text("ma20d12c5"), key: "hide_thinking",
                value: s?.hideThinking.value == true ? "off" : "on",
                selected: s.map { !$0.hideThinking.value } ?? false,
                overriddenBy: s?.hideThinking.overriddenBy ?? []
            ),
            toggle(L10n.text("mea93d6a2"), key: "cot_tool_details", s?.cotToolDetails),
            toggle(L10n.text("m4387e5d3"), key: "badge_effort", s?.badgeEffort),
        ]
        let menubar = [
            toggle(L10n.text("mc3ea5bf7"), key: "show_counts", s.map { .init(value: $0.showCounts, overriddenBy: []) }),
            toggle(
                L10n.text("m7f88308e"), key: "launch_at_login",
                s.map { .init(value: $0.launchAtLogin, overriddenBy: []) }
            ),
        ]
        return [
            SettingRow(id: "sentinel", label: L10n.text("m8ea9bd0d"), choices: sentinel),
            SettingRow(id: "feishu", label: L10n.text("m448eff16"), choices: feishu),
            SettingRow(
                id: "idle_retire",
                label: presetLabel(L10n.text("m2b92a365"), s?.idleRetireSecs.value, autoExitPresets),
                choices: presets(autoExitPresets, key: "idle_retire_secs", current: s?.idleRetireSecs)
            ),
            SettingRow(id: "menubar", label: L10n.text("m2d9de7e6"), choices: menubar),
        ]
    }

    private static func toggle(
        _ title: String,
        key: String,
        _ current: MenubarSettings.Layered<Bool>?
    ) -> SettingChoice {
        let on = current?.value ?? false
        return SettingChoice(
            title: title, key: key, value: on ? "off" : "on",
            selected: on, overriddenBy: current?.overriddenBy ?? []
        )
    }

    private static func presets(
        _ presets: [(String, UInt64)],
        key: String,
        current: MenubarSettings.Layered<UInt64>?
    ) -> [SettingChoice] {
        presets.map { title, secs in
            SettingChoice(
                title: title, key: key, value: String(secs),
                selected: current?.value == secs, overriddenBy: current?.overriddenBy ?? []
            )
        }
    }

    /// 手改出来的值不在预设里时,标签上写出它,哪个胶囊都不亮。
    private static func presetLabel(_ label: String, _ value: UInt64?, _ presets: [(String, UInt64)]) -> String {
        guard let value, !presets.contains(where: { $0.1 == value }) else { return label }
        return "\(label) (\(FleetFormat.uptimeText(value)))"
    }
}
