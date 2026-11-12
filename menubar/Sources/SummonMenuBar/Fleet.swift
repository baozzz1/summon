import Foundation

/// Menu Bar 的统一三态。**由 Rust 下发,Swift 不再自己推导。**
///
/// 2026-08-09 之前这三态是在 Swift 里从 busy/liveness 算出来的,而飞书端即将成为第三个消费者
/// —— 再抄一份就是三份手抄,且三个端不会有人同时看,漂了更难发现(`last_active_text` 就是
/// 前车之鉴)。判据现在只存在于 `summon-core/src/observe/fleet.rs` 一处。
///
/// 未知取值解码成 `.exited` 而不是抛错:新 Rust + 旧 Menu Bar 时,把没见过的状态显示成
/// 「未启动/已退出」是**保守**的 —— 它不会把一个空闲会话说成正在工作。
enum MenuState: String, Codable, Equatable, Sendable {
    case working
    case idle
    case exited
    /// 路由已退休但进程还在 —— teardown 泄漏,需要人处理。**必须显式列出**:解码兜底是
    /// `?? .exited`,漏了它就会被静默降级成「已退出」,等于把事故从菜单里抹掉。
    case leaked

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = MenuState(rawValue: raw) ?? .exited
    }
}

/// `summon agents status --json` 的 fleet 契约，加上 `summon menubar sessions --json` 的
/// 外部会话缓存。Rust 侧仍是唯一事实源；Swift 只负责把两个只读投影视为一份菜单快照。
struct DaemonStatus: Codable, Equatable, Sendable {
    let running: Bool
    let pid: UInt32?
    let version: String?
    let uptimeSecs: UInt64?

    enum CodingKeys: String, CodingKey {
        case running
        case pid
        case version
        case uptimeSecs = "uptime_secs"
    }
}

struct ChatStatus: Codable, Equatable, Sendable {
    let tag: String
    /// 会话的唯一标识(`soul-<alias>-<tag>`)。
    let target: String
    let name: String?
    let chatType: String?
    let busy: Bool
    let capture: String
    let liveness: String
    let lastActiveSecs: UInt64?
    /// Rust 渲染好的英文人读串(Menu Bar 全英文)。Swift 只显示,不再自己分桶(见 `FleetFormat.sinceText`)。
    let lastActiveTextEn: String?
    /// 缺键(更旧的 CLI 不认识 waiting)按 false —— 那时本来就没有 waiting 这回事。
    let waitingFlag: Bool?
    /// Rust 下发的三态。**必需字段**:缺了就该解码失败而不是静默降级 —— 静默降级正是这次
    /// 要消除的东西。menubar 与 summon CLI 由同一次 deploy 一起安装,不存在长期错配。
    let menuState: MenuState
    /// Rust 下发:这是一条「tmux 开着但没在跑」的会话(attach 上去就能接着用)。
    let isIdleTmux: Bool

    enum CodingKeys: String, CodingKey {
        case tag
        case target
        case name
        case chatType = "chat_type"
        case busy
        case capture
        case liveness
        case lastActiveSecs = "last_active_secs"
        case lastActiveTextEn = "last_active_text_en"
        case waitingFlag = "waiting"
        case menuState = "state"
        case isIdleTmux = "idle_tmux"
    }

    /// agent 回完、轮到 owner 处理(Rust 由盘上 waiting 标记得出)。
    var waiting: Bool {
        waitingFlag ?? false
    }

    var displayName: String {
        name ?? tag
    }
}

struct GroupMemberStatus: Codable, Equatable, Sendable {
    let alias: String
    let target: String
    let busy: Bool
    let capture: String
    let liveness: String
    let lastActiveSecs: UInt64?
    let lastActiveTextEn: String?
    /// 见 `ChatStatus.waitingFlag`。
    let waitingFlag: Bool?

    /// Rust 下发的三态(见 `ChatStatus.menuState`)。
    let menuState: MenuState
    /// Rust 下发(见 `ChatStatus.isIdleTmux`)。
    let isIdleTmux: Bool

    enum CodingKeys: String, CodingKey {
        case alias
        case target
        case busy
        case capture
        case liveness
        case lastActiveSecs = "last_active_secs"
        case lastActiveTextEn = "last_active_text_en"
        case waitingFlag = "waiting"
        case menuState = "state"
        case isIdleTmux = "idle_tmux"
    }

    /// 见 `ChatStatus.waiting`。
    var waiting: Bool {
        waitingFlag ?? false
    }
}

struct GroupStatus: Codable, Equatable, Sendable {
    let tag: String
    let name: String?
    let chatType: String?
    let busy: Bool
    let agents: Int
    let lastActiveSecs: UInt64?
    let lastActiveTextEn: String?
    /// 原始 chat_id。缺失 = 这个群的会话路由不回 daemon,行上不给 Exit / Archive。
    let chatId: String?
    /// 打开这个会话的飞书桌面版链接(不定位到消息),由 CLI 下发。
    let link: String?
    /// Rust 下发的群级三态(见 `ChatStatus.menuState`)。
    let menuState: MenuState
    /// Rust 下发:群里有没有「tmux 开着但空闲」的成员。
    let hasIdleTmux: Bool
    let members: [GroupMemberStatus]

    enum CodingKeys: String, CodingKey {
        case tag
        case name
        case chatType = "chat_type"
        case busy
        case agents
        case lastActiveSecs = "last_active_secs"
        case lastActiveTextEn = "last_active_text_en"
        case chatId = "chat_id"
        case link
        case menuState = "state"
        case hasIdleTmux = "idle_tmux"
        case members
    }

    var displayName: String {
        if let name, !name.isEmpty { return name }
        return chatType == "p2p" ? L10n.text("m25f08e63", [String(describing: (tag))]) : tag
    }
}

struct AgentStatus: Codable, Equatable, Sendable {
    let alias: String
    let busy: Bool
    let sessions: Int
    let lastActiveSecs: UInt64?
    let lastActiveTextEn: String?
    let chats: [ChatStatus]

    /// Rust 下发的 agent 级三态(见 `ChatStatus.menuState`)。
    let menuState: MenuState
    /// Rust 下发:该 agent 有没有「tmux 开着但空闲」的会话。
    let hasIdleTmux: Bool

    enum CodingKeys: String, CodingKey {
        case alias
        case busy
        case sessions
        case lastActiveSecs = "last_active_secs"
        case lastActiveTextEn = "last_active_text_en"
        case menuState = "state"
        case hasIdleTmux = "idle_tmux"
        case chats
    }
}

enum ExternalSessionProvider: String, Codable, CaseIterable, Sendable {
    case claude
    case cursor
    case codexCli = "codex_cli"
    case codexApp = "codex_app"

    var displayName: String {
        switch self {
        case .claude: L10n.text("m8e7bb2f0")
        case .cursor: L10n.text("m4faa6009")
        case .codexCli: L10n.text("mddbfd197")
        case .codexApp: L10n.text("mea84d65e")
        }
    }
}

enum ExternalSessionState: String, Codable, Sendable {
    case running
    case completed
}

/// summon 不拥有这些会话，只做只读观测；因此没有 `target`，Menu Bar 不提供 kill。
/// 有正式跳转目标的 provider(目前只有 Codex App)带 `link`,点击交给系统打开。
struct ExternalSessionStatus: Codable, Equatable, Sendable {
    let provider: ExternalSessionProvider
    let id: String
    let title: String
    let cwd: String?
    let state: ExternalSessionState
    let busy: Bool
    let lastActiveSecs: UInt64?
    let link: String?

    enum CodingKeys: String, CodingKey {
        case provider
        case id
        case title
        case cwd
        case state
        case busy
        case lastActiveSecs = "last_active_secs"
        case link
    }

    var menuKey: String {
        "\(provider.rawValue):\(id)"
    }

    var menuState: MenuState {
        if busy { return .working }
        return state == .running ? .idle : .exited
    }

    static func decodeList(_ data: Data) throws -> [ExternalSessionStatus] {
        try JSONDecoder().decode([ExternalSessionStatus].self, from: data)
    }
}

/// 菜单里可点的跳转链接。链接来自 CLI(而 CLI 读的是盘上的 waiting 标记),只放行飞书桌面版与
/// Codex 桌面版两家的协议,别的一律不交给系统打开。
enum OpenableLink {
    static let schemes: Set<String> = ["lark", "feishu", "codex"]

    static func url(_ raw: String?) -> URL? {
        guard let raw, let url = URL(string: raw), let scheme = url.scheme?.lowercased(),
              schemes.contains(scheme)
        else { return nil }
        return url
    }
}

/// 「Dashboard…」打开的链接。它是 `summon dashboard open --print` 打出来的一行字,只放行本机回环
/// 上带端口的 http 链接:本机码只在本机地址上能用,别的主机、协议、带用户名的形态一律不交给系统。
enum DashboardLink {
    private static let loopbackHosts: Set<String> = ["127.0.0.1", "localhost", "::1", "[::1]"]

    static func url(_ raw: String?) -> URL? {
        guard let text = raw?.trimmingCharacters(in: .whitespacesAndNewlines),
              let components = URLComponents(string: text),
              components.scheme == "http",
              components.user == nil, components.password == nil,
              let host = components.host, loopbackHosts.contains(host),
              components.port != nil,
              let url = components.url
        else { return nil }
        return url
    }

    /// daemon 不在时签不出本机码,这一行不显示。
    static func offered(daemonRunning: Bool) -> Bool {
        daemonRunning
    }
}

/// 一条 waiting:agent 回完、轮到 owner 处理。会话已退出的也在列(它可能已不在会话列表里)。
struct WaitingItem: Codable, Equatable, Sendable {
    let alias: String
    let tag: String
    let name: String?
    let chatType: String?
    /// 「Handled」按它比较:菜单渲染后 agent 又回了一条,CLI 就不清。
    let replyMessageId: String
    let sinceSecs: UInt64
    let sinceTextEn: String
    let link: String

    enum CodingKeys: String, CodingKey {
        case alias
        case tag
        case name
        case chatType = "chat_type"
        case replyMessageId = "reply_message_id"
        case sinceSecs = "since_secs"
        case sinceTextEn = "since_text_en"
        case link
    }

    var menuKey: String {
        "\(alias)#\(tag)"
    }

    var chatDisplayName: String {
        if let name, !name.isEmpty { return name }
        return chatType == "p2p" ? L10n.text("m25f08e63", [String(describing: (tag))]) : tag
    }
}

/// 文档评论任务支持停止起草；发布需回飞书审批卡确认。
struct CommentTask: Codable, Equatable, Sendable {
    let id: String
    let agent: String
    let title: String
    let stage: String
    let status: String
    let statusEn: String
    let busy: Bool
    let pid: Int32?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case id, agent, title, stage, status, busy, pid, error
        case statusEn = "status_en"
    }
    var line: String {
        let process = pid.map { L10n.text("m5ebd3313", [String(describing: ($0))]) } ?? ""
        return "\(agent) · \(title) · \(statusEn)\(process)"
    }
    var canStop: Bool { ["received", "queued", "drafting", "paused"].contains(stage) }
    /// 终态任务只在 Dashboard 与 CLI 里留档;认不出的新阶段照常列出。
    var isFinished: Bool { ["replied", "closed", "failed", "expired"].contains(stage) }
    var summary: String { [line, error].compactMap { $0 }.joined(separator: " · ") }
}

struct FleetSnapshot: Codable, Equatable, Sendable {
    let language: String?;
    let commentTasks: [CommentTask]
    let commentTasksError: String?
    let daemon: DaemonStatus
    let agents: [AgentStatus]
    let groups: [GroupStatus]
    let externalSessions: [ExternalSessionStatus]
    /// owner sentinel 三态徽标("off"/"dry-run"/"on";旧 CLI 无此字段 → nil)。
    let sentinel: String?
    /// waiting 的 (agent, 会话),等得最久的在前。
    let waiting: [WaitingItem]
    /// 状态栏用哪个 summon 图标(`menubar.icon`);认不出由 `StatusIcons.summon` 回落默认。
    let menubarIcon: String?
    /// 状态栏要不要在图标旁显示计数(`menubar.show_counts`);旧 CLI 不带按显示。
    let menubarShowCounts: Bool

    enum CodingKeys: String, CodingKey {
        case language
        case commentTasks = "comment_tasks"
        case commentTasksError = "comment_tasks_error"
        case daemon
        case agents
        case groups
        case sentinel
        case waiting
        case menubarIcon = "menubar_icon"
        case menubarShowCounts = "menubar_show_counts"
    }

    /// **这些参数刻意都没有默认值。** 2026-08-10 的实测 bug 就是默认值吃出来的:
    /// `replacingExternalSessions` 重建快照时漏传 `sentinel`,它默认成 `nil`,菜单于是恒显
    /// 「sentinel · 未知」—— 而 daemon 行和群列表全都正常,看起来只有那一项坏,极难往
    /// 「快照被重建过」上想。没有默认值,漏传就是编译错误,同类 bug 不会再有第二次。
    init(
        language: String?,
        commentTasks: [CommentTask],
        commentTasksError: String?,
        daemon: DaemonStatus,
        agents: [AgentStatus],
        groups: [GroupStatus],
        externalSessions: [ExternalSessionStatus],
        sentinel: String?,
        waiting: [WaitingItem],
        menubarIcon: String?,
        menubarShowCounts: Bool
    ) {
        self.language = language
        self.commentTasks = commentTasks
        self.commentTasksError = commentTasksError
        self.daemon = daemon
        self.agents = agents
        self.groups = groups
        self.externalSessions = externalSessions
        self.sentinel = sentinel
        self.waiting = waiting
        self.menubarIcon = menubarIcon
        self.menubarShowCounts = menubarShowCounts
    }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        language = try container.decodeIfPresent(String.self, forKey: .language)
        commentTasks = try container.decodeIfPresent([CommentTask].self, forKey: .commentTasks) ?? []
        commentTasksError = try container.decodeIfPresent(String.self, forKey: .commentTasksError)
        daemon = try container.decode(DaemonStatus.self, forKey: .daemon)
        agents = try container.decode([AgentStatus].self, forKey: .agents)
        groups = try container.decode([GroupStatus].self, forKey: .groups)
        externalSessions = []
        sentinel = try container.decodeIfPresent(String.self, forKey: .sentinel)
        waiting = try container.decodeIfPresent([WaitingItem].self, forKey: .waiting) ?? []
        menubarIcon = try container.decodeIfPresent(String.self, forKey: .menubarIcon)
        menubarShowCounts = try container.decodeIfPresent(Bool.self, forKey: .menubarShowCounts) ?? true
    }

    static let empty = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: DaemonStatus(running: false, pid: nil, version: nil, uptimeSecs: nil),
        agents: [],
        groups: [],
        externalSessions: [],
        // 还没拿到过任何快照 → 确实不知道 sentinel 状态,渲成「未知」是诚实的。
        sentinel: nil,
        waiting: [],
        menubarIcon: nil,
        menubarShowCounts: true
    )

    /// working:此刻在干活的 (agent, 会话) 数 —— 在跑一轮,或主轮已收尾、子 agent 还在跑。
    /// 按会话数:一个 agent 同时在三个群干活就是 3。
    var workingSessions: Int {
        agents.reduce(0) { $0 + $1.chats.filter { $0.menuState == .working }.count } + commentTasks.filter(\.busy).count
    }

    /// 此刻在跑一轮的会话数。停机 / 重启打断的是轮,子 agent 跑在 tmux 里不受影响。
    var turnSessions: Int {
        agents.reduce(0) { $0 + $1.chats.filter(\.busy).count } + commentTasks.filter(\.busy).count
    }

    var hasWorkingSession: Bool {
        workingSessions > 0
    }

    /// Menu Bar 列出的评论任务:还在处理或等 owner 批准的。
    var menuCommentTasks: [CommentTask] {
        commentTasks.filter { !$0.isFinished }
    }

    var menuState: MenuState {
        if hasWorkingSession { return .working }
        if !menuCommentTasks.isEmpty { return .idle }
        if agents.contains(where: { $0.menuState == .idle }) { return .idle }
        return .exited
    }

    static func decode(_ data: Data) throws -> FleetSnapshot {
        try JSONDecoder().decode(FleetSnapshot.self, from: data)
    }

    /// 换掉外部会话列表,**其余一切原样保留**。
    ///
    /// 外部会话来自另一条更慢的命令(`menubar sessions --json`),只在菜单展开/手动刷新时拉,
    /// 所以要在已有快照上就地替换而不是重新取一份。既然是「只换这一项」,别的字段就一个都
    /// 不能在这里丢 —— sentinel 曾经就是这么丢的(见 memberwise init 上的注释)。
    func replacingExternalSessions(_ sessions: [ExternalSessionStatus]) -> FleetSnapshot {
        FleetSnapshot(
            language: language,
            commentTasks: commentTasks,
            commentTasksError: commentTasksError,
            daemon: daemon,
            agents: agents,
            groups: groups,
            externalSessions: sessions,
            sentinel: sentinel,
            waiting: waiting,
            menubarIcon: menubarIcon,
            menubarShowCounts: menubarShowCounts
        )
    }
}

/// 菜单项的**骨架**。
///
/// NSMenu 正在显示时不能重排结构:它的高度已按当时的项数算定,`removeAllItems()` 重来会在底部
/// 留下一块对不上的空白。所以菜单开着时只原地改已有的行;签名变了(多了会话 / 多了 agent /
/// daemon 起停)说明有行要增删,记下来等菜单关闭再重建。会话行的顺序不进签名:排序随状态变,
/// 菜单开着时行留在原位,下次打开再按新状态排。
enum MenuStructure {
    static func signature(fleet: FleetSnapshot) -> String {
        let sessions = SessionRows.build(fleet).map(\.key).sorted().joined(separator: ",")
        let ungrouped = fleet.agents
            .filter { $0.chats.isEmpty }
            .map(\.alias)
            .sorted()
            .joined(separator: ",")
        let external = fleet.externalSessions
            .map(\.menuKey)
            .sorted()
            .joined(separator: ",")
        // daemon 起停会换掉控制项(未运行 →「启动」;运行中 →「重启」「停止」),那是结构变化。
        let daemon = fleet.daemon.running ? "up" : "down"
        let comments = fleet.menuCommentTasks.map { "\($0.id):\($0.stage)" }.sorted().joined(separator: ",")
        return "\(daemon)#\(sessions)#\(ungrouped)#\(external)#\(comments)#\(fleet.commentTasksError ?? "")"
    }
}

enum FleetFormat {
    /// daemon 行:`summon · up 3d 2h`。daemon 不在时诚实说明 —— 此时 busy 一律是 false,
    /// 不能让人误以为「所有 agent 都空闲」。
    static func daemonLine(_ daemon: DaemonStatus) -> String {
        guard daemon.running, let uptime = daemon.uptimeSecs else {
            return L10n.text("m27c6cc96")
        }
        return L10n.text("m329c5782", [String(describing: (uptimeText(uptime)))])
    }

    /// `3d 2h` / `5h 12m` / `42m` / `30s`。与 Rust 侧 `format_uptime` 同一套分桶。
    static func uptimeText(_ secs: UInt64) -> String {
        let minute: UInt64 = 60
        let hour = 60 * minute
        let day = 24 * hour
        if secs >= day {
            let (d, h) = (secs / day, (secs % day) / hour)
            return h == 0 ? "\(d)d" : L10n.text("mb5067bf8", [String(describing: (d)), String(describing: (h))])
        }
        if secs >= hour {
            let (h, m) = (secs / hour, (secs % hour) / minute)
            return m == 0 ? "\(h)h" : L10n.text("md426ce3b", [String(describing: (h)), String(describing: (m))])
        }
        if secs >= minute {
            return "\(secs / minute)m"
        }
        return "\(secs)s"
    }

    /// 「多久前」的唯一取法:优先用 Rust 下发的英文文案,拿不到才回退本地分桶。
    /// 从没活跃过(两个字段都缺)返回 nil,由调用方决定说什么。
    static func activeText(_ text: String?, _ secs: UInt64?) -> String? {
        if let text { return text }
        return secs.map(sinceText)
    }

    /// `just now` / `42s ago` / `12m ago` / `3h ago` / `2d ago` / `>30d`。
    ///
    /// **回退路径**,不是事实源:快照里的相对时间由 Rust `humanize_elapsed_secs_en` 渲染下发。
    /// 它还留着是因为外部会话(`menubar sessions --json`)只有秒数。分桶必须与 Rust 逐档一致 ——
    /// 由 `GoldenContractTests` 拿共享 fixture 的 `last_active_text_en` 逐条钉住,漂了就红。
    static func sinceText(_ secs: UInt64) -> String {
        let minute: UInt64 = 60
        let hour = 60 * minute
        let day = 24 * hour
        if secs < 10 { return "just now" }
        if secs < minute { return "\(secs)s ago" }
        if secs < hour { return "\(secs / minute)m ago" }
        if secs < day { return "\(secs / hour)h ago" }
        if secs <= 30 * day { return "\(secs / day)d ago" }
        return ">30d"
    }

    /// 行尾的短时间:`12m ago` → `12m`,`just now` → `now`。分组已经说明了状态,行尾只要时间。
    static func compactAge(_ text: String?) -> String {
        guard let text else { return "" }
        if text == "just now" { return "now" }
        return text.hasSuffix(" ago") ? String(text.dropLast(4)) : text
    }

    /// 没有会话的 agent 收成一行,只写个数:`No sessions: 6 agents`。名字放进悬停提示
    /// (`idleAgentsTooltip`),agent 一多,名单会把整个菜单撑宽。都有会话时为空串。
    static func idleAgentsLine(_ agents: [AgentStatus]) -> String {
        let count = agents.filter { $0.chats.isEmpty }.count
        switch count {
        case 0: return ""
        case 1: return L10n.text("m07ae160f")
        default: return L10n.text("m8a0148b2", [String(describing: (count))])
        }
    }

    /// 那一行的悬停提示:没有会话的 agent 名单。
    static func idleAgentsTooltip(_ agents: [AgentStatus]) -> String {
        agents.filter { $0.chats.isEmpty }.map(\.alias).joined(separator: " · ")
    }

    /// 外部会话平铺后的一行:`● 检查 Clash 双网络转发… — Codex App · working`。标题过长截断。
    static func externalRowLine(_ session: ExternalSessionStatus) -> String {
        let limit = 40
        let title = session.title.count > limit
            ? String(session.title.prefix(limit)) + "…"
            : session.title
        let line = externalSessionLine(ExternalSessionStatus(
            provider: session.provider, id: session.id, title: title, cwd: session.cwd,
            state: session.state, busy: session.busy, lastActiveSecs: session.lastActiveSecs,
            link: session.link
        ))
        guard let dash = line.range(of: " — ") else { return line }
        return line.replacingCharacters(in: dash, with: " — \(session.provider.displayName) · ")
    }

    static func externalSessionLine(_ session: ExternalSessionStatus) -> String {
        let mark = session.busy ? "●" : "○"
        let state: String
        if session.busy {
            state = "working"
        } else if session.state == .running {
            state = L10n.text("mc8b8bef8")
        } else if let last = session.lastActiveSecs {
            state = L10n.text("m57d9e748", [String(describing: (sinceText(last)))])
        } else {
            state = L10n.text("ma6f82aab")
        }
        return "\(mark) \(session.title) — \(state)"
    }
}
