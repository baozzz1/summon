import AppKit
import Foundation

extension MenuState {
    var menuColor: NSColor {
        switch self {
        case .working: .systemGreen
        case .idle: .secondaryLabelColor
        case .exited: .tertiaryLabelColor
        // 红:与「灰=已退出」区分开。灰是"不用管了",红是"还活着、正等你处理"。
        case .leaked: .systemRed
        }
    }
}

/// 只给行首状态点着色，正文继续服从系统的明暗主题与可访问性设置。
func stateAttributedTitle(_ title: String, state: MenuState) -> NSAttributedString {
    let result = NSMutableAttributedString(string: title)
    guard !title.isEmpty else { return result }
    let firstGlyph = (title as NSString).rangeOfComposedCharacterSequence(at: 0)
    result.addAttribute(.foregroundColor, value: state.menuColor, range: firstGlyph)
    return result
}

/// 状态栏顶层内容必须服从系统主题：暗色菜单栏用白色，亮色菜单栏用黑色。整块内容是一张合成好的
/// 模板图(`StatusBarImage`)，状态文字供辅助功能朗读。`contentTintColor` 会把 NSStatusBarButton 从系统
/// 的动态前景色链路里摘出去，尤其在选中态可能把图标和数字一起压成黑色；这里显式清掉自定义 tint。
@MainActor
func applyStatusBarImage(_ image: NSImage, accessibilityLabel: String, to button: NSButton) {
    button.image = image
    button.imagePosition = .imageOnly
    button.title = ""
    button.contentTintColor = nil
    // macOS 14 起 visibleRect 可超出 bounds；按钮的可见区域需限定在自身范围内。
    if #available(macOS 14.0, *) { button.clipsToBounds = true }
    button.toolTip = nil
    button.setAccessibilityLabel(accessibilityLabel)
}

/// fleet 刷新保持单飞。两类请求不能被正在跑的那一轮吞掉:菜单展开要求的外部发现(在跑的是
/// fleet-only tick),与动作做完后的重取(在跑的那一轮取数早于这次写入)。它们都记下来,那一轮结束后补跑。
struct RefreshGate {
    private(set) var isRefreshing = false
    private var activeIncludesExternal = false
    private var pendingExternal = false
    private var pendingStale = false

    mutating func begin(includeExternal: Bool) -> Bool {
        guard !isRefreshing else {
            if includeExternal, !activeIncludesExternal {
                pendingExternal = true
            }
            return false
        }
        isRefreshing = true
        activeIncludesExternal = includeExternal
        return true
    }

    /// 刚做完一个会改快照的动作。没有在跑的刷新就返回 true,由调用方直接起一轮;正在跑就记下,
    /// 那一轮结束后补跑。
    mutating func invalidate() -> Bool {
        guard isRefreshing else { return true }
        pendingStale = true
        return false
    }

    /// 刚完成的这一轮之后要不要补跑,补跑带不带外部发现。
    mutating func finish() -> (needed: Bool, includeExternal: Bool) {
        defer {
            pendingExternal = false
            pendingStale = false
        }
        isRefreshing = false
        activeIncludesExternal = false
        return (pendingExternal || pendingStale, pendingExternal)
    }
}

/// 底部普通动作只显示文字。显式清空 NSMenuItem 的 image/state-image,避免 AppKit 或未来
/// 公共样式给它补图标后形成额外左侧图标列,破坏与相邻文字项的左对齐。
@MainActor
func textOnlyMenuItem(
    title: String,
    action: Selector?,
    keyEquivalent: String
) -> NSMenuItem {
    let item = NSMenuItem(title: title, action: action, keyEquivalent: keyEquivalent)
    item.image = nil
    item.onStateImage = nil
    item.offStateImage = nil
    item.mixedStateImage = nil
    return item
}

func commentDraftChips(_ task: CommentTask, stopping: Bool) -> [Chip] {
    task.canStop ? [Chip(title: L10n.text("m633337b3"), selected: false, enabled: !stopping,
                        toolTip: L10n.text("md0beb595"))] : []
}

@MainActor
func commentTaskMenuItem(_ task: CommentTask, stopping: Bool = false, onStop: @escaping () -> Void) -> NSMenuItem {
    let item = NSMenuItem()
    let view = ChipRowView(label: task.line, labelColumn: 360,
                           chips: commentDraftChips(task, stopping: stopping),
                           detail: stopping ? L10n.text("mbbe85741") : "") { _ in onStop() }
    view.frame.size.width = ChipLayout.rowWidth(labelColumn: view.labelColumn, chips: view.chips, detail: L10n.text("mbbe85741"))
    view.toolTip = task.summary
    item.view = view
    return item
}

/// 「退出 Menu Bar」项。**绝不能**绑 `NSApplication.terminate(_:)`:macOS 26 会给绑定到系统
/// 标准 selector 的菜单项在**显示时**自动补系统符号图标,构造时清 `image` 挡不住(清的是构造
/// 时刻的值,系统在之后才填)。同菜单里「打开控制台…」用自有 selector 就没图标,是现成对照。
///
/// 连带后果比图标本身更糟:只要菜单里有**任意一项**带图标,AppKit 就为整个菜单预留图标列,
/// 所有标准项被整体右推,与自定义视图的「刷新」错位。所以去掉这个图标同时也是修左对齐。
/// 走自有 selector,由它内部再调 terminate —— 行为完全不变,只是 AppKit 认不出这是标准动作。
@MainActor
func quitMenuItem(target: AnyObject?) -> NSMenuItem {
    let item = textOnlyMenuItem(
        title: L10n.text("mf2901e38"),
        action: #selector(MenuBarController.quitMenuBar),
        keyEquivalent: "q"
    )
    item.target = target
    return item
}

/// 菜单的布局,从上到下:
///
/// 1. 会话一栏:按 Leaked / Waiting / Working / Idle 分组,一个 (群, agent) 一行;没有会话的 agent 收成一行。
/// 2. daemon 行:运行时长 + 起停按钮。
/// 3. 费用估算:本地记录的滚动时间范围。
/// 4. Settings:几行「标签 + 胶囊」。
/// 5. 外部会话(有才出现),平铺、限条数。
/// 6. Dashboard…(daemon 在跑时)/ Refresh / Quit。
///
/// 全部平铺,没有二级菜单;要点的东西都是 view-based 项,点完菜单不关(打开飞书 / Codex 除外)。
@MainActor
final class MenuBarController: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private let client: SummonClient
    private let menu = NSMenu()
    private var statusItem: NSStatusItem?

    private var fleet: FleetSnapshot = .empty
    private var usage: UsageSnapshot?
    private var usageError: String?
    private var usageItems: [String: UsageRowView] = [:]
    private var usageDetailItem: UsageFooterView?
    private var settings: MenubarSettings?
    private var lastError: String?
    private var lastUpdated: Date?

    /// 菜单**正在显示**时不能动它的结构(见 `menuWillOpen`)。这三个状态守着这条规矩。
    private var isMenuOpen = false
    private var pendingRebuild = false
    private var renderedSignature = ""

    /// 会话行,按 `SessionRow.key` 索引(一个 (群, agent) 一行)。
    private var rowViews: [String: SessionRowView] = [:]
    private var commentItems: [String: NSMenuItem] = [:]
    private var stoppingComments: Set<String> = []
    /// 每个分组标题下渲染了哪些行;标题上的数 = 其中还在列表里的。
    private var sectionHeaders: [RowState: (header: SectionHeaderView, keys: [String])] = [:]
    /// 动作还没回来的行及其动作:同一行不叠第二个动作,菜单重建后照样显示进度。
    private var rowsInFlight: [String: SessionAction] = [:]
    /// Exit / Archive 已交给 daemon、会话还在列表里收尾的行。菜单关了再开也不再给动作 ——
    /// 被投第二次会走离线拆除,往群里多发一条退出回执。会话从列表里消失或过了时限就放开。
    private var closedRows: [String: (text: String, since: Date)] = [:]
    private static let closedRowHold: TimeInterval = 120
    private var optionHeld = false
    private var optionKeyTimer: Timer?
    private var idleAgentsItem: NSMenuItem?

    private var daemonRowView: ChipRowView?
    private var daemonActionInFlight = false
    private var daemonDetail = ""
    private var settingRowViews: [String: ChipRowView] = [:]
    private var settingsHeader: SectionHeaderView?
    /// 最近一次读设置失败的原因;读成功就清掉。
    private var settingsError: String?
    /// 正在写的设置行:胶囊先置灰,直到写完、重新取回的值到了才放开,中间不闪回旧值。
    private var settingsInFlight: Set<String> = []
    private var externalItems: [String: NSMenuItem] = [:]
    private var refreshItemView: MenuButtonView?

    private var backgroundTimer: Timer?
    private var refreshGate = RefreshGate()

    /// 只在菜单关着时跑:让状态栏的图标/数字跟得上。菜单一旦打开,取数的节拍交回给人(刷新按钮)。
    private static let backgroundInterval: TimeInterval = 10
    /// 外部会话最多列几条;多出来的报一个数。
    private static let externalLimit = 8

    init(client: SummonClient = SummonClient()) {
        self.client = client
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.menu = menu
        menu.delegate = self
        // 自己管 enabled:默认的 autoenables 只看 target/action,会把「没有会话所以不该点」
        // 这类语义判断覆盖掉。菜单项能不能点是业务决定,不是 AppKit 的。
        menu.autoenablesItems = false
        statusItem = item

        applyStatusItem()
        rebuildMenu()
        refreshNow()
        backgroundTimer = Timer.scheduledTimer(
            withTimeInterval: Self.backgroundInterval,
            repeats: true
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.refreshIfMenuClosed() }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        backgroundTimer?.invalidate()
    }

    // MARK: - 菜单生命周期

    /// `menuWillOpen` 在菜单**显示之前**调用 —— 这是唯一能安全重排结构的时机。
    ///
    /// 这里**只用缓存里的快照**排结构,不取数:取数要 spawn 一个 summon 进程(几十毫秒),同步做
    /// 就是让菜单在鼠标底下卡住那么久。改为立刻用已有数据弹出、同时后台去取,回来后原地改文字
    /// —— 结构在显示前已定死,后到的数据不会重排它,也就不会有底部那块空白。
    func menuWillOpen(_ menu: NSMenu) {
        isMenuOpen = true
        optionHeld = NSEvent.modifierFlags.contains(.option)
        rebuildMenu()
        startWatchingOptionKey()
        refreshNow(includeExternal: true)
    }

    func menuDidClose(_ menu: NSMenu) {
        isMenuOpen = false
        stopWatchingOptionKey()
        if pendingRebuild {
            pendingRebuild = false
            rebuildMenu()
        }
    }

    // MARK: - 取数

    /// 定时器用:只在菜单**关着**时跑,唯一职责是让状态栏的图标/数字跟得上。
    ///
    /// 菜单开着时一律不自动取数 —— 那时人正盯着某一行看它变不变,数据在脚底下自己换掉会让
    /// 「我点刷新之前它是什么样」变得不可知。观察的节拍交给手动的刷新按钮。
    private func refreshIfMenuClosed() {
        guard !isMenuOpen else { return }
        refreshNow(includeExternal: false)
    }

    /// 取一次数(始终异步 —— 取数要 spawn 进程,绝不放在主线程上挡着 UI)。外部会话与设置
    /// 只在菜单展开 / 手动刷新时取:前者慢,后者只有菜单里看得到。
    private func refreshNow(includeExternal: Bool = true) {
        guard refreshGate.begin(includeExternal: includeExternal) else { return }
        refreshItemView?.detail = L10n.text("m1c0def7b")
        Task {
            let client = self.client
            let result = await Task.detached {
                () -> (
                    Result<FleetSnapshot, any Error>,
                    Result<[ExternalSessionStatus], any Error>?,
                    Result<MenubarSettings, any Error>?,
                    Result<UsageSnapshot, any Error>?
                ) in
                (
                    Result { try client.fleetStatus() },
                    includeExternal ? Result { try client.externalSessions() } : nil,
                    includeExternal ? Result { try client.settings() } : nil,
                    includeExternal ? Result { try client.usage() } : nil
                )
            }.value

            let followUp = self.refreshGate.finish()
            switch result.0 {
            case let .success(fleet):
                if let language = fleet.language, L10n.state.setLanguage(language) {
                    self.pendingRebuild = true
                }
                let external = result.1
                    .flatMap { try? $0.get() }
                    ?? self.fleet.externalSessions
                self.fleet = fleet.replacingExternalSessions(external)
                self.lastError = nil
            case let .failure(error):
                // 保留上次快照(比清空有用),只把错误显示在状态行。
                self.lastError = error.localizedDescription
            }
            switch result.2 {
            case let .success(settings):
                self.settings = settings
                self.settingsError = nil
            case let .failure(error):
                self.settingsError = error.localizedDescription
            case nil:
                break
            }
            switch result.3 {
            case let .success(usage):
                self.usage = usage
                self.usageError = nil
            case let .failure(error):
                self.usageError = error.localizedDescription
            case nil:
                break
            }
            self.lastUpdated = Date()
            self.apply()
            if followUp.needed {
                self.refreshNow(includeExternal: followUp.includeExternal)
            }
        }
    }

    /// 动作做完后的重取:正在跑的那一轮取数早于这次写入,结束后补跑,不会被它吞掉。
    private func refreshAfterChange() {
        if refreshGate.invalidate() {
            refreshNow()
        }
    }

    /// 新数据落到 UI 上。菜单关着时可以放心重建;菜单开着时**只原地改已有的项** —— 显示中的菜单
    /// 绝不能重排结构(高度已按当时的项数算定,重排会在底部留一块对不上的空白)。要增删的行
    /// 记下来,等菜单关闭再重建;消失的会话行原地变灰。
    private func apply() {
        pruneClosedRows(listed: Set(SessionRows.build(fleet).map(\.key)))
        applyStatusItem()
        refreshItemView?.detail = refreshDetail()
        guard isMenuOpen else {
            rebuildMenu()
            return
        }
        updateInPlace()
        if currentSignature() != renderedSignature {
            pendingRebuild = true
        }
    }

    // MARK: - 会话行动作

    /// 链接由 CLI 下发(Rust 拼),这里只交给系统:飞书走 `lark://`,Codex 走 `codex://`。
    @objc func openLink(_ sender: NSMenuItem) {
        guard let url = OpenableLink.url(sender.representedObject as? String) else { return }
        NSWorkspace.shared.open(url)
    }

    /// 点会话行的群名那一侧:跳回飞书,菜单随之关闭。
    private func openRow(_ row: SessionRow) {
        guard let url = OpenableLink.url(row.link) else { return }
        menu.cancelTracking()
        NSWorkspace.shared.open(url)
    }

    /// 行尾按钮与 ⌥ 退出。菜单不关:行尾显示进度,完成后写结果并刷新。
    private func runSessionAction(_ action: SessionAction, row: SessionRow) {
        guard rowsInFlight[row.key] == nil else { return }
        switch action {
        case .handled:
            markHandled(row)
        case .exit, .archive:
            endSession(row, action: action, interrupt: false)
        }
    }

    private func markHandled(_ row: SessionRow) {
        guard let item = row.waiting else { return }
        beginAction(.handled, on: row)
        Task {
            let client = self.client
            let result = await Task.detached { Result { try client.markHandled(item) } }.value
            switch result {
            case .success(.superseded):
                self.finishAction(on: row, status: .normal)
                self.showNotice(
                    L10n.text("m85f22a89"),
                    L10n.text("m6169a599", [String(describing: (item.alias))])
                )
            case .success:
                self.finishAction(on: row, status: .done(.handled))
            case let .failure(error):
                self.finishAction(on: row, status: .normal)
                self.showError(error.localizedDescription)
            }
            self.refreshAfterChange()
        }
    }

    /// Exit / Archive。在跑一轮的会话先确认:菜单取数时就在跑的,点下去就问;菜单开着期间才开跑的,
    /// CLI 按此刻的真值回 `.running`,再问一次。确认过才带 `interrupt`。
    private func endSession(_ row: SessionRow, action: SessionAction, interrupt: Bool) {
        if EndSessionFlow.confirmFirst(row, interrupt: interrupt) {
            guard confirmEndRunning(action, row: row) else { return }
            endSession(row, action: action, interrupt: true)
            return
        }
        beginAction(action, on: row)
        Task {
            let client = self.client
            let result = await Task.detached {
                Result { try client.endSession(row, archive: action == .archive, interrupt: interrupt) }
            }.value
            switch result {
            case let .success(outcome):
                switch EndSessionFlow.next(after: outcome, action: action) {
                case .confirmAndRetry:
                    self.finishAction(on: row, status: .normal)
                    if self.confirmEndRunning(action, row: row) {
                        self.endSession(row, action: action, interrupt: true)
                        return
                    }
                case let .show(status):
                    if case let .closed(text) = status {
                        self.closedRows[row.key] = (text, Date())
                    }
                    self.finishAction(on: row, status: status)
                }
            case let .failure(error):
                self.finishAction(on: row, status: .normal)
                self.showError(error.localizedDescription)
            }
            self.refreshAfterChange()
        }
    }

    /// 会话已从列表里消失(收尾完成)或过了时限的,不再压着它的动作。
    private func pruneClosedRows(listed: Set<String>) {
        let now = Date()
        closedRows = closedRows.filter { key, entry in
            listed.contains(key) && now.timeIntervalSince(entry.since) < Self.closedRowHold
        }
    }

    private func beginAction(_ action: SessionAction, on row: SessionRow) {
        rowsInFlight[row.key] = action
        rowViews[row.key]?.status = .running(action)
    }

    /// 期间菜单若关过又开,`rowViews` 里已是重建后的那一行,结果照样落在它上面。
    private func finishAction(on row: SessionRow, status: SessionRowStatus) {
        rowsInFlight[row.key] = nil
        rowViews[row.key]?.status = status
    }

    private func confirmEndRunning(_ action: SessionAction, row: SessionRow) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = SessionConfirm.title(action)
        alert.informativeText = SessionConfirm.body(action, row: row)
        alert.addButton(withTitle: SessionConfirm.confirmButton(action))
        alert.addButton(withTitle: L10n.text("m19766ed6"))
        return alert.runModal() == .alertFirstButtonReturn
    }

    /// 行视图不是第一响应者,收不到 flagsChanged;菜单开着时定时看一眼修饰键,让 ⌥ 一按下行就变样。
    /// 计时器挂在 common 模式里,菜单 tracking 期间照样走。真正的判据是松开鼠标那一刻事件上的修饰键。
    private func startWatchingOptionKey() {
        stopWatchingOptionKey()
        let timer = Timer(timeInterval: 0.05, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.syncOptionKey() }
        }
        RunLoop.main.add(timer, forMode: .common)
        optionKeyTimer = timer
    }

    private func stopWatchingOptionKey() {
        optionKeyTimer?.invalidate()
        optionKeyTimer = nil
    }

    private func syncOptionKey() {
        let held = NSEvent.modifierFlags.contains(.option)
        guard held != optionHeld else { return }
        optionHeld = held
        for view in rowViews.values {
            view.optionHeld = held
        }
    }

    // MARK: - daemon 与设置

    /// 退出的真身。只为绕开 macOS 26 对系统标准 selector 的自动加图标(见 `quitMenuItem`),
    /// 行为与直接绑 `terminate:` 一致。非 private:`quitMenuItem` 要取它的 selector。
    @objc func quitMenuBar() {
        NSApp.terminate(nil)
    }

    /// 起/停/重启 daemon。破坏性的两个先弹确认框,且**拿 busy 真值说代价**(会打断几个在跑的轮)。
    ///
    /// CLI 会阻塞到有结果(start 最多等 5s 就绪),所以整个调用扔到后台线程 —— 主线程要留着刷 UI。
    /// 完成后 daemon 行写一句交代;真正的证据是同一行原地变成「summon · up 2s」。
    private func runDaemonAction(_ action: DaemonAction) {
        guard !daemonActionInFlight else { return }
        if action.needsConfirmation, !confirmDaemonAction(action) {
            return
        }

        NSLog(L10n.text("mfb4def67"), action.command)
        daemonActionInFlight = true
        daemonDetail = action.runningDetail
        configureDaemonRow()
        Task {
            let client = self.client
            let outcome = await Task.detached { () -> (any Error)? in
                do {
                    try client.daemonControl(action)
                    return nil
                } catch {
                    return error
                }
            }.value

            self.daemonActionInFlight = false
            if let outcome {
                NSLog(L10n.text("m032f1089"), action.command, outcome.localizedDescription)
                self.daemonDetail = ""
                // CLI 的报错已经带上了「查看日志: ~/.summon/run/daemon.log」,原样呈上。
                self.showError(outcome.localizedDescription)
            } else {
                NSLog(L10n.text("m3a7b2d68"), action.command)
                self.daemonDetail = action.doneDetail
            }
            self.configureDaemonRow()
            self.refreshAfterChange()
        }
    }

    private func confirmDaemonAction(_ action: DaemonAction) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = DaemonConfirm.title(action)
        alert.informativeText = DaemonConfirm.body(action, turnSessions: fleet.turnSessions)
        alert.addButton(withTitle: DaemonConfirm.confirmButton(action))
        alert.addButton(withTitle: L10n.text("m19766ed6"))
        return alert.runModal() == .alertFirstButtonReturn
    }

    /// 点设置区的一个胶囊:写进 config.yaml(开机自启走 autostart)。写完当场重取设置,取回后放开这一行;
    /// 快照另经 `refreshAfterChange` 重取,状态栏计数随之更新。
    private func applySetting(_ choice: SettingChoice, row: SettingRow) {
        guard !settingsInFlight.contains(row.id) else { return }
        settingsInFlight.insert(row.id)
        configureSettingRows()
        Task {
            let client = self.client
            let (written, reread) = await Task.detached {
                () -> (Result<Void, any Error>, Result<MenubarSettings, any Error>) in
                let written = Result { try client.setSetting(key: choice.key, value: choice.value) }
                return (written, Result { try client.settings() })
            }.value
            switch reread {
            case let .success(settings):
                self.settings = settings
                self.settingsError = nil
            case let .failure(error):
                self.settingsError = error.localizedDescription
            }
            self.settingsInFlight.remove(row.id)
            self.configureSettingRows()
            if case let .failure(error) = written {
                self.showError(error.localizedDescription)
            }
            self.refreshAfterChange()
        }
    }

    // MARK: - 渲染

    /// 刷新项右侧的灰字:正在取数 → 「Refreshing…」;取完 → 「16:52:31」。
    /// 点一下必须看得见有事发生 —— 哪怕数据一模一样,时间戳也变了,那就是「刷过了」的证据。
    private func refreshDetail() -> String {
        if refreshGate.isRefreshing {
            return L10n.text("m1c0def7b")
        }
        guard let lastUpdated else { return "" }
        return Self.clockFormatter.string(from: lastUpdated)
    }

    private static let clockFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "HH:mm:ss"
        return formatter
    }()

    /// 状态栏:summon 图标,开着计数时再加 working / waiting 两段;合成一张模板图由系统按明暗着色。
    /// 辅助功能标签始终包含会话计数。
    private func applyStatusItem() {
        guard let button = statusItem?.button else { return }
        let image = StatusBarImage.compose(
            summon: StatusIcons.summon(named: fleet.menubarIcon),
            working: fleet.menubarShowCounts ? fleet.workingSessions : 0,
            waiting: fleet.menubarShowCounts ? fleet.waiting.count : 0
        )
        let accessibilityLabel = StatusBarImage.accessibilityLabel(
            working: fleet.workingSessions,
            waiting: fleet.waiting.count,
            daemonRunning: fleet.daemon.running,
            leaked: fleet.menuState == .leaked
        )
        applyStatusBarImage(image, accessibilityLabel: accessibilityLabel, to: button)
    }

    private func currentSignature() -> String {
        MenuStructure.signature(fleet: fleet)
    }

    /// daemon 行的标签:有错就报错,否则报 daemon uptime。
    private func statusLine() -> String {
        if let lastError {
            return clipped(lastError, limit: 60)
        }
        return FleetFormat.daemonLine(fleet.daemon)
    }

    private func updateInPlace() {
        // 会话行按 (agent, 会话) 认:还在的换上最新的标题与动作;不在了的原地变灰。
        let rows = Dictionary(
            SessionRows.build(fleet).map { ($0.key, $0) },
            uniquingKeysWith: { first, _ in first }
        )
        for (key, view) in rowViews {
            if let row = rows[key] {
                view.configure(row)
            } else {
                view.markGone()
            }
        }
        for (header, keys) in sectionHeaders.values {
            header.configure(count: keys.filter { rows[$0] != nil }.count)
        }
        idleAgentsItem?.title = FleetFormat.idleAgentsLine(fleet.agents)
        idleAgentsItem?.toolTip = FleetFormat.idleAgentsTooltip(fleet.agents)
        configureDaemonRow()
        configureSettingRows()
        configureUsageItems()
        let tasks = Dictionary(fleet.menuCommentTasks.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        for (id, item) in commentItems {
            guard let view = item.view as? ChipRowView else { continue }
            if let task = tasks[id] {
                if !task.canStop { stoppingComments.remove(id) }
                let stopping = stoppingComments.contains(id)
                view.configure(label: task.line, chips: commentDraftChips(task, stopping: stopping), detail: stopping ? L10n.text("mbbe85741") : "")
                view.toolTip = task.summary
            } else {
                stoppingComments.remove(id)
                view.configure(label: L10n.text("m9b5fea76"), chips: [], detail: "")
            }
        }
        for session in fleet.externalSessions {
            guard let item = externalItems[session.menuKey] else { continue }
            applyStateTitle(FleetFormat.externalRowLine(session), state: session.menuState, to: item)
        }
    }

    private func rebuildMenu() {
        menu.removeAllItems()
        rowViews.removeAll()
        commentItems.removeAll()
        sectionHeaders.removeAll()
        idleAgentsItem = nil
        daemonRowView = nil
        settingRowViews.removeAll()
        settingsHeader = nil
        externalItems.removeAll()
        usageItems.removeAll()
        usageDetailItem = nil

        addSessionSection()
        let commentTasks = fleet.menuCommentTasks
        if !commentTasks.isEmpty || fleet.commentTasksError != nil {
            menu.addItem(.separator())
            addDisabled(L10n.text("m6e8fe775"))
            if let error = fleet.commentTasksError { addDisabled(error) }
            for task in commentTasks {
                let item = commentTaskMenuItem(task, stopping: stoppingComments.contains(task.id)) { [weak self] in
                    MainActor.assumeIsolated { self?.stopComment(task.id) }
                }
                menu.addItem(item)
                commentItems[task.id] = item
            }
        }
        menu.addItem(.separator())
        addDaemonRow()
        menu.addItem(.separator())
        addUsageSection()
        menu.addItem(.separator())
        addSettingsSection()
        addExternalSection()
        menu.addItem(.separator())
        if DashboardLink.offered(daemonRunning: fleet.daemon.running) {
            menu.addItem(makeDashboardItem())
        }
        menu.addItem(makeRefreshItem())
        menu.addItem(quitMenuItem(target: self))

        renderedSignature = currentSignature()
    }

    /// 会话一栏:按组排,每组一个标题;群名、agent 两列在全部行之间对齐(见 `SessionColumns`)。
    private func addSessionSection() {
        guard !fleet.agents.isEmpty else {
            addDisabled(L10n.text("mea2d1a80"))
            return
        }
        let rows = SessionRows.build(fleet)
        let columns = SessionColumns.fitting(rows)
        for (state, members) in SessionRows.sections(rows) {
            let header = SectionHeaderView(
                title: state.sectionTitle,
                count: members.count,
                color: state == .leaked ? .systemRed : .secondaryLabelColor
            )
            let item = NSMenuItem()
            item.view = header
            menu.addItem(item)
            sectionHeaders[state] = (header, members.map(\.key))
            for row in members {
                menu.addItem(makeSessionRowItem(row, columns: columns))
            }
        }
        if rows.isEmpty {
            addDisabled(L10n.text("m1f59d3a2"))
        }
        let idle = FleetFormat.idleAgentsLine(fleet.agents)
        if !idle.isEmpty {
            idleAgentsItem = addDisabled(idle)
            idleAgentsItem?.toolTip = FleetFormat.idleAgentsTooltip(fleet.agents)
        }
    }

    private func makeSessionRowItem(_ row: SessionRow, columns: SessionColumns) -> NSMenuItem {
        let item = NSMenuItem()
        let view = SessionRowView(
            row: row,
            columns: columns,
            onOpen: { [weak self] row in
                MainActor.assumeIsolated { self?.openRow(row) }
            },
            onAction: { [weak self] action, row in
                MainActor.assumeIsolated { self?.runSessionAction(action, row: row) }
            }
        )
        view.optionHeld = optionHeld
        if let action = rowsInFlight[row.key] {
            view.status = .running(action)
        } else if let closed = closedRows[row.key] {
            view.status = .closed(closed.text)
        }
        rowViews[row.key] = view
        item.view = view
        return item
    }

    /// daemon 行:左边是运行时长(或错误),右边是此刻做得了的起停动作。
    private func addDaemonRow() {
        let actions = daemonActions
        let view = ChipRowView(
            label: statusLine(),
            labelColumn: ChipLayout.labelWidth(statusLine()),
            chips: daemonChips(actions),
            detail: daemonDetail
        ) { [weak self] index in
            MainActor.assumeIsolated {
                guard let self, index < self.daemonActions.count else { return }
                self.runDaemonAction(self.daemonActions[index])
            }
        }
        daemonRowView = view
        let item = NSMenuItem()
        item.view = view
        menu.addItem(item)
    }

    /// 只列此刻做得了的事:没运行 →「Start」;运行中 →「Restart…」「Stop…」。
    private var daemonActions: [DaemonAction] {
        DaemonAction.allCases.filter { $0.isAvailable(daemonRunning: fleet.daemon.running) }
    }

    private func daemonChips(_ actions: [DaemonAction]) -> [Chip] {
        actions.map {
            Chip(title: $0.chipTitle, selected: false, enabled: !daemonActionInFlight, toolTip: nil)
        }
    }

    private func configureDaemonRow() {
        daemonRowView?.configure(
            label: statusLine(),
            chips: daemonChips(daemonActions),
            detail: daemonDetail
        )
    }

    private func addUsageSection() {
        let header = NSMenuItem()
        header.view = SectionHeaderView(title: L10n.text("m661dffe6"))
        menu.addItem(header)
        for (key, label) in UsageWindow.labels {
            let item = NSMenuItem()
            let view = UsageRowView(label: label, amount: L10n.text("mba3bbbe1"), tokens: "—")
            item.view = view
            menu.addItem(item)
            usageItems[key] = view
        }
        let detailItem = NSMenuItem()
        let footer = UsageFooterView()
        detailItem.view = footer
        menu.addItem(detailItem)
        usageDetailItem = footer
        configureUsageItems()
    }

    private func configureUsageItems() {
        for (key, _) in UsageWindow.labels {
            let window = usage?.windows.first { $0.key == key }
            let amount = usageError != nil ? L10n.text("mca184496") : window?.amount ?? L10n.text("mba3bbbe1")
            usageItems[key]?.configure(amount: amount, tokens: usageError != nil ? "—" : window?.tokenUsage ?? "—", tooltip: window?.tooltip)
        }
        usageDetailItem?.configure(
            note: usageError == nil ? usage?.note ?? L10n.text("m9b264091") : L10n.text("m3e53b591"),
            updated: usageError == nil ? usage?.detail ?? "" : L10n.text("mf3f726bd"),
            tooltip: usageError ?? usage?.tooltip
        )
    }

    /// Settings:几行「标签 + 胶囊」,标签列等宽,胶囊上下对齐。还没取到设置时胶囊都灰着。
    private func addSettingsSection() {
        let header = NSMenuItem()
        let headerView = SectionHeaderView(title: L10n.text("m74a883a0"))
        header.view = headerView
        settingsHeader = headerView
        menu.addItem(header)
        let rows = SettingsLayout.rows(settings)
        let labelColumn = rows.map { ChipLayout.labelWidth($0.label) }.max() ?? 0
        for row in rows {
            let view = ChipRowView(
                label: row.label,
                labelColumn: labelColumn,
                chips: settingChips(row),
                detail: settingDetail(row)
            ) { [weak self] index in
                MainActor.assumeIsolated { self?.settingChipClicked(rowID: row.id, index: index) }
            }
            settingRowViews[row.id] = view
            let item = NSMenuItem()
            item.view = view
            menu.addItem(item)
        }
        configureSettingRows()
    }

    /// 按点击那一刻的设置值算要写什么:行的数据可能在菜单开着时被刷新过。
    private func settingChipClicked(rowID: String, index: Int) {
        guard let row = SettingsLayout.rows(settings).first(where: { $0.id == rowID }),
              index < row.choices.count
        else { return }
        applySetting(row.choices[index], row: row)
    }

    /// 设置读不出来时胶囊都灰着:手里的值可能已经过时,照着它点会写错。
    private func settingChips(_ row: SettingRow) -> [Chip] {
        let enabled = settings != nil && settingsError == nil && !settingsInFlight.contains(row.id)
        return row.choices.map { choice in
            Chip(
                title: choice.title,
                selected: choice.selected,
                enabled: enabled,
                toolTip: choice.overriddenBy.isEmpty
                    ? nil
                    : L10n.text("m2375596e", [String(describing: (choice.overriddenBy.joined(separator: ", ")))])
            )
        }
    }

    private func settingDetail(_ row: SettingRow) -> String {
        if settingsInFlight.contains(row.id) { return L10n.text("m23e39291") }
        let agents = row.overriddenBy
        switch agents.count {
        case 0: return ""
        case 1: return L10n.text("m14823b94", [String(describing: (agents[0]))])
        default: return L10n.text("m3c0b68d9", [String(describing: (agents.count))])
        }
    }

    /// 设置读不出来时胶囊一直灰着;原因写在 Settings 标题旁,全文在悬停提示里。
    private func configureSettingRows() {
        settingsHeader?.configure(
            detail: settingsError == nil ? "" : L10n.text("m829a2a46"),
            toolTip: settingsError
        )
        for row in SettingsLayout.rows(settings) {
            settingRowViews[row.id]?.configure(
                label: row.label,
                chips: settingChips(row),
                detail: settingDetail(row)
            )
        }
    }

    /// 外部会话平铺:有跳转目标的(Codex App)点了交给系统打开,其余只读。最多列 `externalLimit` 条。
    private func addExternalSection() {
        let sessions = fleet.externalSessions
        guard !sessions.isEmpty else { return }
        menu.addItem(.separator())
        let header = NSMenuItem()
        header.view = SectionHeaderView(title: L10n.text("m68c114ea"), count: sessions.count)
        menu.addItem(header)
        for session in sessions.prefix(Self.externalLimit) {
            let item = NSMenuItem(title: "", action: nil, keyEquivalent: "")
            if let link = session.link {
                item.action = #selector(openLink(_:))
                item.target = self
                item.representedObject = link
                item.isEnabled = true
                item.toolTip = L10n.text("m4e26d4b7")
            } else {
                item.isEnabled = false
                item.toolTip = [session.title, session.cwd].compactMap { $0 }.joined(separator: "\n")
            }
            applyStateTitle(FleetFormat.externalRowLine(session), state: session.menuState, to: item)
            externalItems[session.menuKey] = item
            menu.addItem(item)
        }
        if sessions.count > Self.externalLimit {
            addDisabled(L10n.text("m49a9f24b", [String(describing: (sessions.count - Self.externalLimit))]))
        }
    }

    private func applyStateTitle(_ title: String, state: MenuState, to item: NSMenuItem?) {
        guard let item else { return }
        item.title = title
        item.attributedTitle = stateAttributedTitle(title, state: state)
    }

    private func stopComment(_ id: String) {
        guard let task = fleet.commentTasks.first(where: { $0.id == id }), task.canStop,
              stoppingComments.insert(id).inserted else { return }
        updateInPlace()
        Task {
            let client = self.client
            let result = await Task.detached { Result { try client.stopComment(task) } }.value
            if case let .failure(error) = result {
                self.stoppingComments.remove(id)
                self.showError(error.localizedDescription)
                self.updateInPlace()
            }
            self.refreshAfterChange()
        }
    }

    /// 点了要去浏览器,菜单随之关闭,所以是普通菜单项。链接经 CLI 现要(一次性、60 秒内有效)。
    private func makeDashboardItem() -> NSMenuItem {
        let item = textOnlyMenuItem(
            title: L10n.text("m4b32277e"),
            action: #selector(openDashboard(_:)),
            keyEquivalent: ""
        )
        item.target = self
        return item
    }

    @objc func openDashboard(_ sender: NSMenuItem) {
        Task {
            let client = self.client
            let result = await Task.detached { Result { try client.dashboardURL() } }.value
            switch result {
            case let .success(url):
                NSWorkspace.shared.open(url)
            case let .failure(error):
                self.showError(error.localizedDescription)
            }
        }
    }

    private func makeRefreshItem() -> NSMenuItem {
        let item = NSMenuItem()
        let view = MenuButtonView(title: L10n.text("m0e916101"), detail: refreshDetail()) { [weak self] in
            MainActor.assumeIsolated { self?.refreshNow() }
        }
        refreshItemView = view
        item.view = view
        return item
    }

    @discardableResult
    private func addDisabled(_ title: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.isEnabled = false
        menu.addItem(item)
        return item
    }

    private func showNotice(_ title: String, _ message: String) {
        let alert = NSAlert()
        alert.alertStyle = .informational
        alert.messageText = title
        alert.informativeText = message
        alert.runModal()
    }

    private func showError(_ message: String) {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = L10n.text("mb6626797")
        alert.informativeText = message
        alert.runModal()
    }

    private func clipped(_ value: String, limit: Int) -> String {
        let oneLine = value.replacingOccurrences(of: "\n", with: " ")
        guard oneLine.count > limit else { return oneLine }
        return String(oneLine.prefix(limit)) + "…"
    }
}
