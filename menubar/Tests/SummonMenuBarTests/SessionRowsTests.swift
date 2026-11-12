import Foundation
import Testing
@testable import SummonMenuBar

private func member(
    _ alias: String,
    tag: String,
    state: MenuState,
    waiting: Bool = false
) -> GroupMemberStatus {
    GroupMemberStatus(
        alias: alias, target: "soul-\(alias)-\(tag)", busy: state == .working, capture: "rpc",
        liveness: "recent", lastActiveSecs: 60, lastActiveTextEn: "1m ago",
        waitingFlag: waiting, menuState: state, isIdleTmux: false
    )
}

private func group(
    _ tag: String,
    name: String,
    chatId: String? = nil,
    members: [GroupMemberStatus]
) -> GroupStatus {
    GroupStatus(
        tag: tag, name: name, chatType: "group", busy: members.contains(where: \.busy),
        agents: members.count, lastActiveSecs: 60, lastActiveTextEn: "1m ago",
        chatId: chatId ?? "oc_\(tag)", link: "lark://group/\(tag)", menuState: .idle,
        hasIdleTmux: false, members: members
    )
}

private func waiting(_ alias: String, tag: String, name: String, since: UInt64) -> WaitingItem {
    WaitingItem(
        alias: alias, tag: tag, name: name, chatType: "group",
        replyMessageId: "om_\(alias)_\(tag)", sinceSecs: since, sinceTextEn: "\(since / 60)m ago",
        link: "lark://reply/\(alias)/\(tag)"
    )
}

private func fleet(
    groups: [GroupStatus],
    waiting: [WaitingItem] = [],
    daemonRunning: Bool = true
) -> FleetSnapshot {
    FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: DaemonStatus(
            running: daemonRunning, pid: daemonRunning ? 1 : nil, version: nil,
            uptimeSecs: daemonRunning ? 60 : nil
        ),
        agents: [], groups: groups, externalSessions: [], sentinel: "off",
        waiting: waiting, menubarIcon: nil, menubarShowCounts: true
    )
}

/// waiting 与会话列表合成一栏:同一个 (群, agent) 只出现一次,waiting 的那行点开跳到那条回复。
@Test func waitingMergesIntoItsSessionRow() throws {
    let golden = try FleetSnapshot.decode(Data(contentsOf: GoldenFixture.fleetStatusURL()))
    let rows = SessionRows.build(golden)

    #expect(rows.filter { $0.key == "saturn#g1a2b3c4" }.count == 1, "waiting 的会话不能出现两次")
    let saturn = try #require(rows.first { $0.key == "saturn#g1a2b3c4" })
    #expect(saturn.waiting?.replyMessageId == "om_golden_reply")
    #expect(saturn.link == golden.waiting[0].link, "waiting 行跳到那条回复")
    #expect(saturn.actions == [.handled, .exit, .archive])
    #expect(saturn.state == .waiting)
    #expect(saturn.age == "12m", "waiting 行的时间从那条回复算起")

    let starlight = try #require(rows.first { $0.key == "starlight#g1a2b3c4" })
    #expect(starlight.waiting == nil)
    #expect(starlight.link == "lark://applink.feishu.cn/client/chat/open?openChatId=oc_g1a2b3c4",
            "其余行打开群")
    #expect(starlight.actions == [.exit, .archive])
    #expect(starlight.busy)

    let memberCount = golden.groups.reduce(0) { $0 + $1.members.count }
    #expect(rows.count == memberCount, "每个成员一行,waiting 不额外加行")
}

/// 会话已经不在列表里、只剩 waiting:照样列出,只能 Handled。
@Test func waitingWhoseSessionIsGoneStillGetsARow() throws {
    let snapshot = fleet(
        groups: [group("g1", name: "研发群", members: [member("saturn", tag: "g1", state: .idle)])],
        waiting: [waiting("arcadia", tag: "g9", name: "杂项群", since: 3_600)]
    )
    let rows = SessionRows.build(snapshot)
    let gone = try #require(rows.first { $0.key == "arcadia#g9" })
    #expect(gone.actions == [.handled], "会话已不在,没有可退出或归档的")
    #expect(gone.state == .waiting)
    #expect(gone.summary == "杂项群 · arcadia · waiting · 60m")
    #expect(gone.link == "lark://reply/arcadia/g9")
}

/// 要处理的在前:leaked → waiting(等得最久的在前,含会话已不在的)→ working → idle;
/// 同一组内保持 CLI 下发的顺序。
@Test func rowsPutWhatNeedsAttentionFirst() {
    let snapshot = fleet(
        groups: [
            group("g1", name: "一群", members: [
                member("idle1", tag: "g1", state: .idle),
                member("work1", tag: "g1", state: .working),
                member("waitNew", tag: "g1", state: .idle, waiting: true),
            ]),
            group("g2", name: "二群", members: [
                member("idle2", tag: "g2", state: .idle),
                member("leak", tag: "g2", state: .leaked),
                member("work2", tag: "g2", state: .working),
                member("waitOld", tag: "g2", state: .idle, waiting: true),
            ]),
        ],
        // CLI 下发的 waiting 已按等得最久的在前排好。
        waiting: [
            waiting("waitOld", tag: "g2", name: "二群", since: 7_200),
            waiting("gone", tag: "g9", name: "九群", since: 3_600),
            waiting("waitNew", tag: "g1", name: "一群", since: 60),
        ]
    )
    #expect(SessionRows.build(snapshot).map(\.alias) == [
        "leak", "waitOld", "gone", "waitNew", "work1", "work2", "idle1", "idle2",
    ])
}

/// 在跑一轮的 waiting 行算 working(标题写的也是 working),但仍可 Handled。
@Test func aWorkingRowThatIsAlsoWaitingSortsWithWorking() throws {
    let snapshot = fleet(
        groups: [group("g1", name: "研发群", members: [
            member("idle", tag: "g1", state: .idle),
            member("busy", tag: "g1", state: .working, waiting: true),
        ])],
        waiting: [waiting("busy", tag: "g1", name: "研发群", since: 600)]
    )
    let rows = SessionRows.build(snapshot)
    #expect(rows.map(\.alias) == ["busy", "idle"])
    #expect(rows[0].actions == [.handled, .exit, .archive])
    #expect(rows[0].state == .working)
}

/// 没有 chat_id 路由不回 daemon:不给 Exit / Archive。daemon 没在跑:给,但灰掉。
@Test func exitAndArchiveNeedAChatIdAndARunningDaemon() throws {
    var noChatId = group("g1", name: "研发群", members: [member("a", tag: "g1", state: .idle)])
    noChatId = GroupStatus(
        tag: noChatId.tag, name: noChatId.name, chatType: noChatId.chatType, busy: false,
        agents: 1, lastActiveSecs: nil, lastActiveTextEn: nil, chatId: nil, link: nil,
        menuState: .idle, hasIdleTmux: false, members: noChatId.members
    )
    #expect(SessionRows.build(fleet(groups: [noChatId]))[0].actions.isEmpty)

    let down = SessionRows.build(fleet(
        groups: [group("g1", name: "研发群", members: [member("a", tag: "g1", state: .idle)])],
        daemonRunning: false
    ))[0]
    #expect(down.actions == [.exit, .archive])
    #expect(!down.canRun(.exit) && !down.canRun(.archive), "daemon 没在跑时结束不了会话")
}

/// 菜单开着时行留在原位:只是状态变了(排序随之变)不算结构变化,行的增减才算。
@Test func signatureIgnoresRowOrderButNotRowSet() {
    let idle = fleet(groups: [group("g1", name: "研发群", members: [
        member("a", tag: "g1", state: .idle),
        member("b", tag: "g1", state: .idle),
    ])])
    let reordered = fleet(groups: [group("g1", name: "研发群", members: [
        member("a", tag: "g1", state: .idle),
        member("b", tag: "g1", state: .working),
    ])])
    let fewer = fleet(groups: [group("g1", name: "研发群", members: [
        member("a", tag: "g1", state: .idle),
    ])])
    #expect(SessionRows.build(idle).map(\.alias) != SessionRows.build(reordered).map(\.alias))
    #expect(MenuStructure.signature(fleet: idle) == MenuStructure.signature(fleet: reordered))
    #expect(MenuStructure.signature(fleet: idle) != MenuStructure.signature(fleet: fewer))
}

/// 确认框写明是谁的哪条会话、会打断什么、之后怎样。
@Test func confirmNamesTheChatAndAgentAndWhatHappensNext() throws {
    let row = try #require(SessionRows.build(fleet(groups: [group("p1", name: "", members: [
        member("arcadia", tag: "p1", state: .working),
    ])])).first)
    #expect(SessionConfirm.title(.exit) == "Exit this session?")
    #expect(SessionConfirm.confirmButton(.archive) == "Archive")
    let exit = SessionConfirm.body(.exit, row: row)
    #expect(exit.hasPrefix("p1 · arcadia is running a turn."))
    #expect(exit.contains("continues this conversation"))
    let archive = SessionConfirm.body(.archive, row: row)
    #expect(archive.contains("starts a new session"))
}

/// 菜单取数时就在跑:点下去先问;人确认过(带 interrupt)不再问。
@Test func endSessionAsksFirstOnlyForARowSeenRunning() throws {
    let rows = SessionRows.build(fleet(groups: [group("g1", name: "研发群", members: [
        member("busy", tag: "g1", state: .working),
        member("idle", tag: "g1", state: .idle),
    ])]))
    let busy = try #require(rows.first { $0.alias == "busy" })
    let idle = try #require(rows.first { $0.alias == "idle" })
    #expect(EndSessionFlow.confirmFirst(busy, interrupt: false))
    #expect(!EndSessionFlow.confirmFirst(busy, interrupt: true))
    #expect(!EndSessionFlow.confirmFirst(idle, interrupt: false))
}

/// CLI 回来之后:`running` 再问一次;其余结果一律收成 closed,这一行不会被投第二次。
@Test func endSessionOutcomesMapToTheNextStep() {
    #expect(EndSessionFlow.next(after: .running, action: .exit) == .confirmAndRetry)
    #expect(EndSessionFlow.next(after: .ended, action: .exit) == .show(.closed("Exited")))
    #expect(EndSessionFlow.next(after: .ended, action: .archive) == .show(.closed("Archived")))
    #expect(EndSessionFlow.next(after: .pending, action: .archive) == .show(.closed("Still ending…")))
    #expect(EndSessionFlow.next(after: .absent, action: .exit) == .show(.closed("Already ended")))
}

/// 分组按 leaked → waiting → working → idle,空组不出现;waiting 与 idle 是两组、两种颜色。
@Test func sectionsFollowTheStatesAndSkipEmptyOnes() {
    let snapshot = fleet(
        groups: [group("g1", name: "研发群", members: [
            member("idle", tag: "g1", state: .idle),
            member("wait", tag: "g1", state: .idle, waiting: true),
        ])],
        waiting: [waiting("wait", tag: "g1", name: "研发群", since: 600)]
    )
    let sections = SessionRows.sections(SessionRows.build(snapshot))
    #expect(sections.map(\.state) == [.waiting, .idle])
    #expect(sections.map { $0.rows.map(\.alias) } == [["wait"], ["idle"]])
}

/// 认不出的状态(解码兜底成 exited)显示为 exited、单独成组;claude 的 tmux 空闲在悬停提示里写明。
@Test func exitedHasItsOwnGroupAndTmuxIdleIsNamed() throws {
    var gone = member("gone", tag: "g1", state: .exited)
    let tmux = GroupMemberStatus(
        alias: "tmux", target: "soul-tmux-g1", busy: false, capture: "transcript", liveness: "live",
        lastActiveSecs: 60, lastActiveTextEn: "1m ago", waitingFlag: false, menuState: .idle,
        isIdleTmux: true
    )
    gone = GroupMemberStatus(
        alias: gone.alias, target: gone.target, busy: false, capture: gone.capture,
        liveness: gone.liveness, lastActiveSecs: gone.lastActiveSecs,
        lastActiveTextEn: gone.lastActiveTextEn, waitingFlag: false, menuState: .exited,
        isIdleTmux: false
    )
    let rows = SessionRows.build(fleet(groups: [group("g1", name: "研发群", members: [gone, tmux])]))
    #expect(rows.map(\.state) == [.idle, .exited])
    #expect(rows[0].summary == "研发群 · tmux · Tmux idle · 1m")
    #expect(rows[1].summary == "研发群 · gone · exited · 1m")
}

// MARK: - 设置区:点哪个胶囊写什么、哪个亮

private func settings(
    sentinel: String = "on",
    showCounts: Bool = true,
    launchAtLogin: Bool = false,
    hideThinking: Bool = false,
    cotVisible: Bool = true,
    idleRetire: UInt64 = 0,
    overriddenBy: [String] = []
) -> MenubarSettings {
    MenubarSettings(
        sentinel: sentinel, showCounts: showCounts, launchAtLogin: launchAtLogin,
        cotVisible: .init(value: cotVisible, overriddenBy: overriddenBy),
        hideThinking: .init(value: hideThinking, overriddenBy: []),
        cotToolDetails: .init(value: false, overriddenBy: []),
        badgeEffort: .init(value: true, overriddenBy: []),
        idleRetireSecs: .init(value: idleRetire, overriddenBy: [])
    )
}

private func choice(_ rows: [SettingRow], _ id: String, _ title: String) throws -> SettingChoice {
    let row = try #require(rows.first { $0.id == id })
    return try #require(row.choices.first { $0.title == title })
}

/// 配置存的是「隐藏思考」,胶囊说的是「显示思考」:亮 = 显示;点下去写的是翻转后的「隐藏」值。
@Test func thinkingChipIsTheInverseOfHideThinking() throws {
    let shown = try choice(SettingsLayout.rows(settings(hideThinking: false)), "feishu", "Thinking")
    #expect(shown.key == "hide_thinking")
    #expect(shown.selected)
    #expect(shown.value == "on", "现在显示着,点一下 = 隐藏")
    let hidden = try choice(SettingsLayout.rows(settings(hideThinking: true)), "feishu", "Thinking")
    #expect(!hidden.selected)
    #expect(hidden.value == "off", "现在隐藏着,点一下 = 不再隐藏")
}

/// 多选胶囊点下去写的是翻转后的值;覆盖了这一项的 agent 带在胶囊上。
@Test func toggleChipsWriteTheFlippedValue() throws {
    let rows = SettingsLayout.rows(settings(showCounts: true, launchAtLogin: false, cotVisible: true,
                                            overriddenBy: ["saturn"]))
    let process = try choice(rows, "feishu", "Process")
    #expect((process.key, process.value, process.selected) == ("cot_visible", "off", true))
    #expect(process.overriddenBy == ["saturn"])
    #expect(try #require(rows.first { $0.id == "feishu" }).overriddenBy == ["saturn"])
    let counts = try choice(rows, "menubar", "Counts")
    #expect((counts.key, counts.value, counts.selected) == ("show_counts", "off", true))
    let login = try choice(rows, "menubar", "Launch at login")
    #expect((login.key, login.value, login.selected) == ("launch_at_login", "on", false))
}

/// 单选行恰好亮一个,写的是这个胶囊自己的值。
@Test func singleChoiceRowsLightExactlyTheCurrentValue() throws {
    for mode in ["off", "dry-run", "on"] {
        let row = try #require(SettingsLayout.rows(settings(sentinel: mode)).first { $0.id == "sentinel" })
        #expect(row.choices.filter(\.selected).map(\.value) == [mode])
        #expect(row.choices.allSatisfy { $0.key == "sentinel" })
    }
    let rows = SettingsLayout.rows(settings(idleRetire: 0))
    let off = try choice(rows, "idle_retire", "Off")
    #expect((off.key, off.value, off.selected) == ("idle_retire_secs", "0", true))
}

/// 手改出来、不在预设里的值:标签写出它,哪个胶囊都不亮;还没取到设置时全都不亮、标签不带数值。
@Test func offPresetValuesAndMissingSettingsLightNothing() throws {
    let rows = SettingsLayout.rows(settings(idleRetire: 3_300))
    let idle = try #require(rows.first { $0.id == "idle_retire" })
    #expect(idle.label == "Auto-exit idle (55m)")
    #expect(idle.choices.allSatisfy { !$0.selected })
    #expect(rows.map(\.id) == ["sentinel", "feishu", "idle_retire", "menubar"])
    #expect(rows.flatMap(\.choices).allSatisfy { $0.key != "idle_ttl_secs" })

    let empty = SettingsLayout.rows(nil)
    #expect(empty.map(\.id) == SettingsLayout.rows(settings()).map(\.id), "结构与取到后相同")
    #expect(empty.flatMap(\.choices).allSatisfy { !$0.selected })
    #expect(empty.first { $0.id == "idle_retire" }?.label == "Auto-exit idle")
}
