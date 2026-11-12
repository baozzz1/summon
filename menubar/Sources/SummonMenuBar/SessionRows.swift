import Foundation

/// 会话行上的动作。全部转发给 summon CLI,Menu Bar 自己不碰会话、不读标记文件。
enum SessionAction: String, CaseIterable, Sendable {
    case handled
    case exit
    case archive

    var buttonTitle: String {
        switch self {
        case .handled: L10n.text("m1e7536d5")
        case .exit: L10n.text("md17d84a6")
        case .archive: L10n.text("m66f4804e")
        }
    }

    /// 进行中的右侧提示。
    var runningDetail: String {
        switch self {
        case .handled: L10n.text("m98a436e5")
        case .exit: L10n.text("mc018d81b")
        case .archive: L10n.text("maf9e1c99")
        }
    }

    /// 行尾图标按钮用的 SF Symbol。
    var symbolName: String {
        switch self {
        case .handled: "checkmark.circle"
        case .exit: "rectangle.portrait.and.arrow.right"
        case .archive: "archivebox"
        }
    }

    /// 鼠标停在按钮上时的提示。
    var toolTip: String {
        switch self {
        case .handled: L10n.text("m03084f4e")
        case .exit: L10n.text("m11a792e6")
        case .archive: L10n.text("m68bbd369")
        }
    }

    /// 完成后的右侧提示。行在菜单关闭前留在原位,给它一个交代。
    var doneDetail: String {
        switch self {
        case .handled: L10n.text("m1e7536d5")
        case .exit: L10n.text("m85fc46c1")
        case .archive: L10n.text("mbdb86505")
        }
    }
}

/// 会话行的展示状态,也是会话一栏的分组。waiting 来自盘上标记(Rust 下发),其余来自 Rust 的三态。
///
/// 在干活(在跑一轮,或子 agent 还在跑)的 waiting 算 working。会话已不在列表里、只剩 waiting 的也算 waiting。
/// 认不出的状态(`MenuState` 解码兜底成 `.exited`)显示为 exited,单独成组。
enum RowState: Int, CaseIterable, Sendable {
    case leaked
    case waiting
    case working
    case idle
    case exited

    var sectionTitle: String {
        switch self {
        case .leaked: L10n.text("md790c902")
        case .waiting: L10n.text("m6e293a8c")
        case .working: L10n.text("ma92f0449")
        case .idle: L10n.text("mab0171ca")
        case .exited: L10n.text("m85fc46c1")
        }
    }

    /// 悬停提示与读屏用的状态词。
    var word: String {
        switch self {
        case .leaked: "leaked"
        case .waiting: "waiting"
        case .working: "working"
        case .idle: "idle"
        case .exited: "exited"
        }
    }
}

/// 菜单里的一行会话:一个 (群, agent)。waiting 与会话列表合成一栏,同一件事只出现一次。
struct SessionRow: Equatable, Sendable {
    let alias: String
    let tag: String
    /// 群名(私聊是 `DM · <tag>`)。
    let chatName: String
    let state: RowState
    /// 行尾的「多久前」:waiting 行从那条回复算起,其余行是最后活跃。去掉了 ` ago`,省宽度。
    let age: String
    /// 菜单取数那一刻是否在跑一轮。只用来决定要不要先问;执行时 CLI 会按真值再判一次。
    let busy: Bool
    /// claude 会话 tmux 开着但空闲(attach 上去就能接着用),悬停提示里写 `Tmux idle`。
    let tmuxIdle: Bool
    let waiting: WaitingItem?
    /// 点行打开的链接:waiting 跳到那条回复,其余打开群。
    let link: String?
    /// 这一行有的动作,按按钮从左到右的顺序。
    let actions: [SessionAction]
    /// 有、但此刻做不了的动作(daemon 没在跑时的 Exit / Archive):画成灰的,点了没反应。
    let disabled: Set<SessionAction>

    /// 与 `WaitingItem.menuKey` 同形:waiting 行与会话行按它认成同一行。
    var key: String {
        "\(alias)#\(tag)"
    }

    /// 一句话说全这一行:悬停提示与读屏用。
    var summary: String {
        let word = state == .idle && tmuxIdle ? L10n.text("m1bd77404") : state.word
        return [chatName, alias, word, age].filter { !$0.isEmpty }.joined(separator: " · ")
    }

    func canRun(_ action: SessionAction) -> Bool {
        actions.contains(action) && !disabled.contains(action)
    }
}

enum SessionRows {
    /// 合成一栏:每个群成员一行,再补上会话已不在列表里、只剩 waiting 的那些。
    ///
    /// 顺序按分组:leaked → waiting → working → idle。waiting 组内等得最久的在前(CLI 已排好);
    /// 其余组内保持 CLI 下发的顺序(同群的会话挨着)。
    static func build(_ fleet: FleetSnapshot) -> [SessionRow] {
        let waitingOrder = Dictionary(
            fleet.waiting.enumerated().map { ($0.element.menuKey, $0.offset) },
            uniquingKeysWith: { first, _ in first }
        )
        let waitingByKey = Dictionary(
            fleet.waiting.map { ($0.menuKey, $0) },
            uniquingKeysWith: { first, _ in first }
        )

        var rows: [SessionRow] = []
        var seen = Set<String>()
        for group in fleet.groups {
            for member in group.members {
                let key = "\(member.alias)#\(group.tag)"
                guard seen.insert(key).inserted else { continue }
                let waiting = member.waiting ? waitingByKey[key] : nil
                var actions: [SessionAction] = waiting == nil ? [] : [.handled]
                var disabled = Set<SessionAction>()
                // 没有 chat_id 就路由不回 daemon,不给 Exit / Archive。
                if group.chatId != nil {
                    actions += [.exit, .archive]
                    if !fleet.daemon.running {
                        disabled = [.exit, .archive]
                    }
                }
                let state: RowState = switch member.menuState {
                case .leaked: .leaked
                case .working: .working
                case .idle: waiting == nil ? .idle : .waiting
                case .exited: waiting == nil ? .exited : .waiting
                }
                let age = state == .waiting
                    ? waiting?.sinceTextEn
                    : FleetFormat.activeText(member.lastActiveTextEn, member.lastActiveSecs)
                rows.append(SessionRow(
                    alias: member.alias,
                    tag: group.tag,
                    chatName: group.displayName,
                    state: state,
                    age: FleetFormat.compactAge(age),
                    busy: member.busy,
                    tmuxIdle: member.isIdleTmux,
                    waiting: waiting,
                    link: waiting?.link ?? group.link,
                    actions: actions,
                    disabled: disabled
                ))
            }
        }
        for item in fleet.waiting where seen.insert(item.menuKey).inserted {
            rows.append(SessionRow(
                alias: item.alias,
                tag: item.tag,
                chatName: item.chatDisplayName,
                state: .waiting,
                age: FleetFormat.compactAge(item.sinceTextEn),
                busy: false,
                tmuxIdle: false,
                waiting: item,
                link: item.link,
                actions: [.handled],
                disabled: []
            ))
        }
        // 排序键:(组, 组内次序, CLI 下发的次序)。最后一项让同组同次序的行保持原顺序。
        return rows.enumerated()
            .map { offset, row -> (SessionRow, Int, Int) in
                let within = row.state == .waiting ? waitingOrder[row.key] ?? 0 : 0
                return (row, within, offset)
            }
            .sorted { ($0.0.state.rawValue, $0.1, $0.2) < ($1.0.state.rawValue, $1.1, $1.2) }
            .map(\.0)
    }

    /// 按组切开,空组不出现。
    static func sections(_ rows: [SessionRow]) -> [(state: RowState, rows: [SessionRow])] {
        RowState.allCases.compactMap { state in
            let members = rows.filter { $0.state == state }
            return members.isEmpty ? nil : (state, members)
        }
    }
}

/// Exit / Archive 的流程判定:点下去要不要先问、CLI 回来之后下一步做什么。
enum EndSessionFlow {
    enum Next: Equatable {
        /// 菜单打开后它才开跑:按此刻的真值再问一次,确认了带 `--interrupt` 重来。
        case confirmAndRetry
        case show(SessionRowStatus)
    }

    /// 菜单取数时就在跑、人还没确认过:先问。
    static func confirmFirst(_ row: SessionRow, interrupt: Bool) -> Bool {
        row.busy && !interrupt
    }

    /// 交出去之后一律收成 `.closed`:还在收尾的会话被投第二次,会再往群里发一条退出回执。
    static func next(after outcome: EndSessionOutcome, action: SessionAction) -> Next {
        switch outcome {
        case .running: .confirmAndRetry
        case .ended: .show(.closed(action.doneDetail))
        case .pending: .show(.closed(L10n.text("m27de2566")))
        case .absent: .show(.closed(L10n.text("md391c386")))
        }
    }
}

/// 结束一条正在跑一轮的会话前的确认框文案。写明是谁的哪条会话、会打断什么、之后怎样 ——
/// 一屏几行长得差不多,确认框是唯一能挡住点错行的东西。
enum SessionConfirm {
    static func title(_ action: SessionAction) -> String {
        action == .archive ? L10n.text("m64b2da28") : L10n.text("m618e8bef")
    }

    static func body(_ action: SessionAction, row: SessionRow) -> String {
        let name = "\(row.chatName) · \(row.alias)"
        if action == .archive {
            return L10n.text("mff5dbc3c", [String(describing: (name))])
        }
        return L10n.text("m0d854e2e", [String(describing: (name))])
    }

    static func confirmButton(_ action: SessionAction) -> String {
        action == .archive ? L10n.text("m66f4804e") : L10n.text("md17d84a6")
    }

}
