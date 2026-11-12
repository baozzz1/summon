import Foundation

/// Menu Bar 能对 daemon 做的三件事。全部转发给既有 CLI(`summon daemon start|stop|restart`),
/// Menu Bar 自己不认识 pidfile、flock、也不发信号。
enum DaemonAction: String, CaseIterable, Sendable {
    case start
    case stop
    case restart

    var command: String { rawValue }

    /// 菜单里的标题。停止/重启带省略号 —— macOS 惯例:点了会弹确认框。
    var menuTitle: String {
        switch self {
        case .start: L10n.text("m631ea8ff")
        case .stop: L10n.text("mebffe975")
        case .restart: L10n.text("m18a03dd2")
        }
    }

    /// daemon 行上的按钮文字。停止/重启带省略号 —— 点了会弹确认框。
    var chipTitle: String {
        switch self {
        case .start: L10n.text("me4bb9f1e")
        case .stop: L10n.text("m6b4c3354")
        case .restart: L10n.text("m2c58de95")
        }
    }

    /// 进行中的右侧提示。
    var runningDetail: String {
        switch self {
        case .start: L10n.text("mbbe5fc3b")
        case .stop: L10n.text("mbbe85741")
        case .restart: L10n.text("m75d0f146")
        }
    }

    /// 完成后的右侧提示(菜单结构要等关闭才能重排,所以这一项还会在原地待着,给它一个交代)。
    var doneDetail: String {
        switch self {
        case .start: L10n.text("mecbc89cd")
        case .stop: L10n.text("m1a4f630a")
        case .restart: L10n.text("m170c5874")
        }
    }

    /// 破坏性 = 会打断正在跑的轮,点之前必须确认。
    var needsConfirmation: Bool {
        self != .start
    }

    /// daemon 当前是否在跑,决定这个动作现在有没有意义。
    /// 不显示「三个里灰掉两个」—— 只列此刻做得了的事。
    func isAvailable(daemonRunning: Bool) -> Bool {
        switch self {
        case .start: !daemonRunning
        case .stop, .restart: daemonRunning
        }
    }
}

/// 停止/重启的确认框文案。
///
/// 不写泛泛的「确定吗」—— 拿 working 真值说话,让人**知道自己在付什么代价**。
/// 最后那句关于 tmux 的话同样重要:它回答「停 daemon 会不会把我的 claude 会话干掉」,
/// 而答案是不会(硬规则:tmux 是持久层,跨 daemon 重启存活)。
enum DaemonConfirm {
    static func title(_ action: DaemonAction) -> String {
        switch action {
        case .stop: L10n.text("mca678770")
        case .restart: L10n.text("m8ef2bb03")
        case .start: L10n.text("m80bc0fbd")
        }
    }

    static func body(_ action: DaemonAction, turnSessions: Int) -> String {
        let verb = action == .restart ? L10n.text("m40f68cf7") : L10n.text("ma71ee1d4")
        let cost: String
        if turnSessions == 0 {
            cost = L10n.text("ma126a159")
        } else {
            let subject = turnSessions == 1
                ? L10n.text("m58005731")
                : L10n.text("mc9c766ce", [String(describing: (turnSessions))])
            let object = turnSessions == 1 ? L10n.text("m2ad8a704") : L10n.text("mc9a8dc33")
            cost = L10n.text("mb4bac9c3", [String(describing: (subject)), String(describing: (verb)), String(describing: (object))])
        }
        return L10n.text("mff798af4", [String(describing: (cost))])
    }

    static func confirmButton(_ action: DaemonAction) -> String {
        action == .restart ? L10n.text("m6b983a81") : L10n.text("mcae7d57b")
    }
}
