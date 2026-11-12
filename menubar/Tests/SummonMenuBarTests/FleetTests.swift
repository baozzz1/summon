import AppKit
import Foundation
import Testing
@testable import SummonMenuBar

/// 契约测试:这段 JSON 是 `summon agents status --json` 的真实输出形态(字段名对齐 Rust 侧
/// `summon_core::fleet`)。它变了这里必须先红。
private let liveJSON = """
{
  "daemon": {
    "running": true,
    "pid": 4242,
    "version": "0.1.0",
    "uptime_secs": 269400
  },
  "agents": [
    {
      "alias": "starlight",
      "busy": true,
      "sessions": 2,
      "last_active_secs": 12,
      "chats": [
        {
          "tag": "g1a2b3c4",
          "target": "soul-starlight-g1a2b3c4",
          "name": "研发群",
          "busy": true,
          "capture": "transcript",
          "liveness": "live",
          "last_active_secs": 12,
          "state": "working",
          "waiting": false,
          "idle_tmux": false
        },
        {
          "tag": "p9f8e7d6",
          "target": "soul-starlight-p9f8e7d6",
          "busy": false,
          "capture": "rpc",
          "liveness": "recent",
          "last_active_secs": 900,
          "state": "idle",
          "waiting": false,
          "idle_tmux": false
        }
      ],
      "state": "working",
      "idle_tmux": false
    },
    {
      "alias": "sakura",
      "busy": false,
      "sessions": 0,
      "chats": [],
      "state": "exited",
      "idle_tmux": false
    }
  ],
  "groups": [
    {
      "tag": "g1a2b3c4",
      "name": "研发群",
      "chat_type": "group",
      "busy": true,
      "agents": 1,
      "last_active_secs": 12,
      "members": [
        {
          "alias": "starlight",
          "target": "soul-starlight-g1a2b3c4",
          "busy": true,
          "capture": "transcript",
          "liveness": "live",
          "last_active_secs": 12,
          "state": "working",
          "waiting": false,
          "idle_tmux": false
        }
      ],
      "state": "working",
      "idle_tmux": false
    },
    {
      "tag": "p9f8e7d6",
      "chat_type": "p2p",
      "busy": false,
      "agents": 1,
      "last_active_secs": 900,
      "members": [
        {
          "alias": "starlight",
          "target": "soul-starlight-p9f8e7d6",
          "busy": false,
          "capture": "rpc",
          "liveness": "recent",
          "last_active_secs": 900,
          "state": "idle",
          "waiting": false,
          "idle_tmux": false
        }
      ],
      "state": "idle",
      "idle_tmux": false
    }
  ]
}
"""

@Test func decodesFleetSnapshotFromCLIContract() throws {
    let snapshot = try FleetSnapshot.decode(Data(liveJSON.utf8))

    #expect(snapshot.daemon.running)
    #expect(snapshot.daemon.uptimeSecs == 269_400)
    #expect(snapshot.agents.count == 2)
    #expect(snapshot.groups.count == 2)
    #expect(snapshot.workingSessions == 1)

    let starlight = snapshot.agents[0]
    #expect(starlight.busy)
    #expect(starlight.chats[0].target == "soul-starlight-g1a2b3c4")
    #expect(starlight.chats[0].displayName == "研发群")
    // 群名缺席时退回 tag —— 不能显示一行空白。
    #expect(starlight.chats[1].displayName == "p9f8e7d6")
    #expect(snapshot.agents[1].lastActiveSecs == nil)
    #expect(snapshot.externalSessions.isEmpty, "fleet 命令不夹带高开销外部发现")
}

@Test func decodesAndFormatsExternalSessionContract() throws {
    let sessions = try ExternalSessionStatus.decodeList(Data("""
    [
       {"provider":"claude","id":"claude-1","title":"summon 之外的终端会话",
        "cwd":"/repo/a","state":"running","busy":false},
       {"provider":"cursor","id":"cursor-1","title":"project-a",
        "cwd":"/repo/project-a","state":"running","busy":false},
       {"provider":"codex_cli","id":"codex-cli-1","title":"终端任务",
        "cwd":"/repo/cli","state":"running","busy":true,"last_active_secs":2},
       {"provider":"codex_app","id":"codex-running","title":"实现 Menu Bar section",
        "cwd":"/repo/b","state":"running","busy":true,"last_active_secs":3},
       {"provider":"codex_app","id":"codex-done","title":"修复排序",
        "cwd":"/repo/c","state":"completed","busy":false,"last_active_secs":720}]
    """.utf8))

    #expect(sessions.count == 5)
    let claude = sessions[0]
    #expect(claude.menuKey == "claude:claude-1")
    #expect(FleetFormat.externalSessionLine(claude) == "○ summon 之外的终端会话 — process running")
    let codexCLI = sessions.filter { $0.provider == .codexCli }
    #expect(FleetFormat.externalSessionLine(codexCLI[0]) == "● 终端任务 — working")
    let codex = sessions.filter { $0.provider == .codexApp }
    #expect(FleetFormat.externalSessionLine(codex[0]) == "● 实现 Menu Bar section — working")
    #expect(FleetFormat.externalSessionLine(codex[1]) == "○ 修复排序 — completed 12m ago")
    // 平铺后的一行带上 provider,长标题截断。
    #expect(FleetFormat.externalRowLine(codex[1]) == "○ 修复排序 — Codex App · completed 12m ago")
    #expect(FleetFormat.externalRowLine(claude) == "○ summon 之外的终端会话 — Claude Terminal · process running")
    let long = ExternalSessionStatus(
        provider: .codexApp, id: "x", title: String(repeating: "长", count: 60), cwd: nil,
        state: .running, busy: false, lastActiveSecs: nil, link: nil
    )
    #expect(FleetFormat.externalRowLine(long).hasPrefix("○ " + String(repeating: "长", count: 40) + "… — Codex App"))
}

@Test func menuOpenQueuesExternalRefreshBehindFleetOnlyTick() {
    var gate = RefreshGate()

    let startedFleetTick = gate.begin(includeExternal: false)
    let startedConcurrentExternal = gate.begin(includeExternal: true)
    let followUp = gate.finish()
    #expect(startedFleetTick, "后台 fleet-only tick 应开始")
    #expect(!startedConcurrentExternal, "单飞期间不能并发再起 summon 进程")
    #expect(followUp.needed && followUp.includeExternal, "菜单展开要求的外部发现不能被 fleet-only tick 吞掉")

    let startedExternal = gate.begin(includeExternal: true)
    let startedDuplicateExternal = gate.begin(includeExternal: true)
    let redundant = gate.finish()
    #expect(startedExternal, "当前刷新结束后应能立即补跑外部发现")
    #expect(!startedDuplicateExternal, "已包含外部发现时，同类请求直接合并")
    #expect(!redundant.needed, "已在执行的外部发现不应制造多余补跑")
}

/// 动作做完后的重取:没在跑就直接起;正在跑的那一轮取数早于写入,结束后补跑一轮。
@Test func aChangeDuringARefreshIsFollowedUp() {
    var gate = RefreshGate()
    let idleInvalidate = gate.invalidate()
    #expect(idleInvalidate, "没在跑:调用方直接起一轮")

    let started = gate.begin(includeExternal: true)
    let busyInvalidate = gate.invalidate()
    let followUp = gate.finish()
    let again = gate.finish()
    #expect(started)
    #expect(!busyInvalidate, "正在跑:记下,不并发再起")
    #expect(followUp.needed, "写入之后要再取一轮,否则效果要等下一个定时 tick")
    #expect(!followUp.includeExternal)
    #expect(!again.needed, "补跑只记一次")
}

@Test func workingCountsSessionsNotAgents() throws {
    let snapshot = try FleetSnapshot.decode(Data(liveJSON.utf8))
    #expect(snapshot.workingSessions == 1)
    #expect(snapshot.hasWorkingSession)
    #expect(snapshot.menuState == .working)

    // 一个 agent 同时在两个群跑:working 是 2,不是 1。
    let twoChats = try FleetSnapshot.decode(Data("""
    {"daemon":{"running":true,"pid":1,"uptime_secs":5},
     "agents":[{"alias":"arcadia","busy":true,"sessions":2,"state":"working","idle_tmux":false,
       "chats":[
         {"tag":"g1","target":"soul-arcadia-g1","busy":true,"capture":"transcript","liveness":"live",
          "state":"working","waiting":false,"idle_tmux":false},
         {"tag":"g2","target":"soul-arcadia-g2","busy":true,"capture":"transcript","liveness":"live",
          "state":"working","waiting":false,"idle_tmux":false}]}],
     "groups":[]}
    """.utf8))
    #expect(twoChats.workingSessions == 2)
}

/// 主轮已收尾、子 agent 还在跑:Rust 下发 working、busy 为 false。会话行归 Working、状态栏按 working
/// 计;停机提示只数在跑一轮的会话。
@Test func aSessionWhoseSubagentsAreRunningCountsAsWorking() throws {
    let snapshot = try FleetSnapshot.decode(Data("""
    {"daemon":{"running":true,"pid":1,"uptime_secs":5},
     "agents":[{"alias":"arcadia","busy":false,"sessions":1,"state":"working","idle_tmux":false,
       "chats":[
         {"tag":"g1","target":"soul-arcadia-g1","busy":false,"capture":"transcript","liveness":"live",
          "state":"working","waiting":true,"idle_tmux":false}]}],
     "groups":[{"tag":"g1","name":"研发群","busy":false,"agents":1,"state":"working","idle_tmux":false,
       "members":[{"alias":"arcadia","target":"soul-arcadia-g1","busy":false,"capture":"transcript",
         "liveness":"live","waiting":true,"state":"working","idle_tmux":false}]}],
     "waiting":[{"alias":"arcadia","tag":"g1","name":"研发群","reply_message_id":"om_1",
       "since_secs":600,"since_text_en":"10m ago","link":"lark://reply"}]}
    """.utf8))
    #expect(snapshot.workingSessions == 1)
    #expect(snapshot.turnSessions == 0)
    #expect(snapshot.menuState == .working)
    #expect(SessionRows.build(snapshot).map(\.state) == [.working])
}

@Test func emptyFleetHasNothingWorkingOrWaiting() {
    #expect(FleetSnapshot.empty.workingSessions == 0)
    #expect(FleetSnapshot.empty.waiting.isEmpty)
    #expect(!FleetSnapshot.empty.hasWorkingSession)
    #expect(FleetSnapshot.empty.menuState == .exited)
}

@Test func menuStateDistinguishesWorkingIdleAndExited() throws {
    let snapshot = try FleetSnapshot.decode(Data(liveJSON.utf8))
    #expect(snapshot.agents[0].menuState == .working)
    #expect(snapshot.agents[0].chats[0].menuState == .working)
    #expect(snapshot.agents[0].chats[1].menuState == .idle)
    #expect(snapshot.agents[1].menuState == .exited)

    let idle = try FleetSnapshot.decode(Data("""
    {
      "daemon": {
        "running": true,
        "pid": 1,
        "uptime_secs": 10
      },
      "agents": [
        {
          "alias": "starlight",
          "busy": false,
          "sessions": 1,
          "chats": [
            {
              "tag": "g1",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 60,
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ],
      "groups": [
        {
          "tag": "g1",
          "busy": false,
          "agents": 1,
          "members": [
            {
              "alias": "starlight",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 60,
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ]
    }
    """.utf8))
    #expect(idle.menuState == .idle)
    #expect(idle.groups[0].menuState == .idle)
    #expect(idle.groups[0].members[0].menuState == .idle)
}

@Test func semanticDotUsesDistinctColorsWithoutTintingTheWholeTitle() {
    for (state, expected) in [
        (MenuState.working, NSColor.systemGreen),
        (.idle, NSColor.secondaryLabelColor),
        (.exited, NSColor.tertiaryLabelColor),
    ] {
        let title = stateAttributedTitle("● agent — 状态", state: state)
        #expect(title.attribute(.foregroundColor, at: 0, effectiveRange: nil) as? NSColor == expected)
        #expect(title.attribute(.foregroundColor, at: 3, effectiveRange: nil) == nil)
    }
}

@MainActor
@Test func statusBarImageUsesSystemThemeTint() throws {
    let button = NSButton()
    button.contentTintColor = .systemGreen
    button.title = "summon 1/7"

    let image = StatusBarImage.compose(summon: StatusIcons.summon(named: "xyz"), working: 2, waiting: 1)
    applyStatusBarImage(image, accessibilityLabel: "summon · 2 working · 1 waiting", to: button)

    #expect(try #require(button.image).isTemplate)
    #expect(button.contentTintColor == nil)
    #expect(button.title.isEmpty, "状态栏只放图标与数字")
    #expect(button.toolTip == nil)
    #expect(button.accessibilityLabel() == "summon · 2 working · 1 waiting")
}

@Test func degradedSnapshotWithoutDaemonStillRenders() throws {
    // daemon 不在:CLI 仍回合法 JSON(busy 全 false、无 uptime),Menu Bar 不该白屏或崩。
    let json = """
    {
      "daemon": {
        "running": false
      },
      "agents": [
        {
          "alias": "sakura",
          "busy": false,
          "sessions": 1,
          "last_active_secs": 30,
          "chats": [
            {
              "tag": "g1",
              "target": "soul-sakura-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 30,
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ],
      "groups": [
        {
          "tag": "g1",
          "busy": false,
          "agents": 1,
          "last_active_secs": 30,
          "members": [
            {
              "alias": "sakura",
              "target": "soul-sakura-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 30,
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ]
    }
    """
    let snapshot = try FleetSnapshot.decode(Data(json.utf8))

    #expect(!snapshot.daemon.running)
    #expect(snapshot.daemon.pid == nil)
    #expect(snapshot.workingSessions == 0)
    #expect(FleetFormat.daemonLine(snapshot.daemon) == "daemon not running (busy state unknown)")
}

@Test func daemonLineReadsUptime() {
    let daemon = DaemonStatus(running: true, pid: 1, version: nil, uptimeSecs: 3 * 86_400 + 2 * 3_600)
    #expect(FleetFormat.daemonLine(daemon) == "summon · up 3d 2h")
}

@Test func uptimeAndSinceBucketsMatchRustSide() {
    #expect(FleetFormat.uptimeText(30) == "30s")
    #expect(FleetFormat.uptimeText(90) == "1m")
    #expect(FleetFormat.uptimeText(3_600) == "1h")
    #expect(FleetFormat.uptimeText(3_600 + 12 * 60) == "1h 12m")
    #expect(FleetFormat.uptimeText(2 * 86_400) == "2d")
    #expect(FleetFormat.uptimeText(3 * 86_400 + 2 * 3_600) == "3d 2h")

    #expect(FleetFormat.sinceText(5) == "just now")
    #expect(FleetFormat.sinceText(42) == "42s ago")
    #expect(FleetFormat.sinceText(12 * 60) == "12m ago")
    #expect(FleetFormat.sinceText(3 * 3_600) == "3h ago")
}




/// 会话行是一级菜单行:同一个群多一个 agent,菜单就多一行,必须等菜单关闭后重建;
/// 成员只是 waiting 翻转只改那一行,走原地更新。
@Test func sessionRowsDriveMenuStructure() throws {
    let base = try FleetSnapshot.decode(Data(liveJSON.utf8))
    let group = base.groups[0]
    func snapshot(members: [GroupMemberStatus]) -> FleetSnapshot {
        let changed = GroupStatus(
            tag: group.tag, name: group.name, chatType: group.chatType, busy: group.busy,
            agents: members.count, lastActiveSecs: group.lastActiveSecs,
            lastActiveTextEn: group.lastActiveTextEn, chatId: group.chatId, link: group.link,
            menuState: group.menuState,
            hasIdleTmux: group.hasIdleTmux, members: members
        )
        return FleetSnapshot(
            language: nil,
            commentTasks: [],
            commentTasksError: nil,
            daemon: base.daemon, agents: base.agents, groups: [changed] + base.groups.dropFirst(),
            externalSessions: base.externalSessions, sentinel: base.sentinel,
            waiting: base.waiting, menubarIcon: base.menubarIcon, menubarShowCounts: true
        )
    }
    func saturn(waiting: Bool) -> GroupMemberStatus {
        GroupMemberStatus(
            alias: "saturn", target: "soul-saturn-g1a2b3c4", busy: false, capture: "rpc",
            liveness: "recent", lastActiveSecs: 60, lastActiveTextEn: "1m ago",
            waitingFlag: waiting, menuState: .idle, isIdleTmux: false
        )
    }
    let one = snapshot(members: group.members)
    let two = snapshot(members: group.members + [saturn(waiting: false)])
    let twoWaiting = snapshot(members: group.members + [saturn(waiting: true)])

    #expect(MenuStructure.signature(fleet: one) != MenuStructure.signature(fleet: two),
            "同群新增一个 agent = 多一行会话")
    #expect(MenuStructure.signature(fleet: two) == MenuStructure.signature(fleet: twoWaiting),
            "成员 waiting 翻转只改会话行文字")
}


/// 菜单开着时重排结构 = 底部残留一块空白(NSMenu 的高度已按旧项数算定)。签名是那道闸:
/// 只有骨架真的变了才重建,而重建必须等菜单关闭。
@Test func busyChangesDoNotChangeMenuStructure() throws {
    let idle = try FleetSnapshot.decode(Data("""
    {
      "daemon": {
        "running": true,
        "pid": 1,
        "uptime_secs": 10
      },
      "agents": [
        {
          "alias": "starlight",
          "busy": false,
          "sessions": 1,
          "last_active_secs": 900,
          "chats": [
            {
              "tag": "g1",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 900,
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ],
      "groups": [
        {
          "tag": "g1",
          "busy": false,
          "agents": 1,
          "last_active_secs": 900,
          "members": [
            {
              "alias": "starlight",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 900,
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ]
    }
    """.utf8))
    // 同一个 agent、同一个会话,只是开始跑一轮了 —— 骨架不变,菜单只该改文字。
    let busy = try FleetSnapshot.decode(Data("""
    {
      "daemon": {
        "running": true,
        "pid": 1,
        "uptime_secs": 40
      },
      "agents": [
        {
          "alias": "starlight",
          "busy": true,
          "sessions": 1,
          "last_active_secs": 2,
          "chats": [
            {
              "tag": "g1",
              "target": "soul-s-g1",
              "busy": true,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 2,
              "state": "working",
              "waiting": false,
              "idle_tmux": false
            }
          ],
          "state": "working",
          "idle_tmux": false
        }
      ],
      "groups": [
        {
          "tag": "g1",
          "busy": true,
          "agents": 1,
          "last_active_secs": 2,
          "members": [
            {
              "alias": "starlight",
              "target": "soul-s-g1",
              "busy": true,
              "capture": "transcript",
              "liveness": "live",
              "last_active_secs": 2,
              "state": "working",
              "waiting": false,
              "idle_tmux": false
            }
          ],
          "state": "working",
          "idle_tmux": false
        }
      ]
    }
    """.utf8))

    #expect(
        MenuStructure.signature(fleet: idle) ==
            MenuStructure.signature(fleet: busy),
        "busy/时间变化不得改变骨架,否则每次刷新都重建、每次重建都留空白"
    )
}

@Test func newSessionChangesMenuStructure() throws {
    let one = try FleetSnapshot.decode(Data("""
    {
      "daemon": {
        "running": true,
        "pid": 1,
        "uptime_secs": 10
      },
      "agents": [
        {
          "alias": "starlight",
          "busy": false,
          "sessions": 1,
          "chats": [
            {
              "tag": "g1",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ],
      "groups": [
        {
          "tag": "g1",
          "busy": false,
          "agents": 1,
          "members": [
            {
              "alias": "starlight",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ]
    }
    """.utf8))
    let two = try FleetSnapshot.decode(Data("""
    {
      "daemon": {
        "running": true,
        "pid": 1,
        "uptime_secs": 10
      },
      "agents": [
        {
          "alias": "starlight",
          "busy": false,
          "sessions": 2,
          "chats": [
            {
              "tag": "g1",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            },
            {
              "tag": "g2",
              "target": "soul-s-g2",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ],
      "groups": [
        {
          "tag": "g1",
          "busy": false,
          "agents": 1,
          "members": [
            {
              "alias": "starlight",
              "target": "soul-s-g1",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        },
        {
          "tag": "g2",
          "busy": false,
          "agents": 1,
          "members": [
            {
              "alias": "starlight",
              "target": "soul-s-g2",
              "busy": false,
              "capture": "transcript",
              "liveness": "live",
              "state": "idle",
              "waiting": false,
              "idle_tmux": true
            }
          ],
          "state": "idle",
          "idle_tmux": true
        }
      ]
    }
    """.utf8))

    #expect(
        MenuStructure.signature(fleet: one) !=
            MenuStructure.signature(fleet: two),
        "多一个会话 = 多一个菜单项,必须重建"
    )
}

@Test func exitedAgentIdentityChangesMenuStructure() throws {
    let daemon = DaemonStatus(running: true, pid: 1, version: nil, uptimeSecs: 10)
    let sakura = AgentStatus(
        alias: "sakura",
        busy: false,
        sessions: 0,
        lastActiveSecs: nil,
        lastActiveTextEn: nil,
        chats: [],
        menuState: .exited,
        hasIdleTmux: false
    )
    let saturn = AgentStatus(
        alias: "saturn",
        busy: false,
        sessions: 0,
        lastActiveSecs: nil,
        lastActiveTextEn: nil,
        chats: [],
        menuState: .exited,
        hasIdleTmux: false
    )
    let one = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: daemon, agents: [sakura], groups: [], externalSessions: [], sentinel: nil, waiting: [], menubarIcon: nil, menubarShowCounts: true)
    let another = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: daemon, agents: [saturn], groups: [], externalSessions: [], sentinel: nil, waiting: [], menubarIcon: nil, menubarShowCounts: true)

    #expect(
        MenuStructure.signature(fleet: one) !=
            MenuStructure.signature(fleet: another),
        "退出态按 agent 逐行显示，身份变化必须触发安全重建"
    )
}

@Test func externalStateChangesInPlaceButIdentityChangesStructure() throws {
    let daemon = DaemonStatus(running: true, pid: 1, version: nil, uptimeSecs: 10)
    let running = ExternalSessionStatus(
        provider: .codexApp,
        id: "thread-1",
        title: "任务",
        cwd: "/repo",
        state: .running,
        busy: true,
        lastActiveSecs: 1,
        link: nil
    )
    let completed = ExternalSessionStatus(
        provider: .codexApp,
        id: "thread-1",
        title: "任务",
        cwd: "/repo",
        state: .completed,
        busy: false,
        lastActiveSecs: 20,
        link: nil
    )
    let another = ExternalSessionStatus(
        provider: .codexApp,
        id: "thread-2",
        title: "另一任务",
        cwd: "/repo",
        state: .completed,
        busy: false,
        lastActiveSecs: 20,
        link: nil
    )
    let active = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: daemon,
        agents: [],
        groups: [],
        externalSessions: [running],
        sentinel: nil,
        waiting: [],
        menubarIcon: nil, menubarShowCounts: true
    )
    let done = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: daemon,
        agents: [],
        groups: [],
        externalSessions: [completed],
        sentinel: nil,
        waiting: [],
        menubarIcon: nil, menubarShowCounts: true
    )
    let changed = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: daemon,
        agents: [],
        groups: [],
        externalSessions: [completed, another],
        sentinel: nil,
        waiting: [],
        menubarIcon: nil, menubarShowCounts: true
    )

    #expect(
        MenuStructure.signature(fleet: active) ==
            MenuStructure.signature(fleet: done),
        "同一外部会话从运行变完成只改标题，菜单打开时不能重排"
    )
    #expect(
        MenuStructure.signature(fleet: done) !=
            MenuStructure.signature(fleet: changed),
        "外部会话增删会改变菜单项数量，必须等菜单关闭后重建"
    )
}

/// owner 2026-08-10 实测:菜单恒显「sentinel · 未知」,而 daemon 行和群列表**全都正常** ——
/// 看起来像只有那一项坏了,实际是整份快照在合并外部会话时被重建过,`sentinel` 因为
/// memberwise init 的默认值 `nil` 被静默吃掉。
///
/// 外部会话每次展开菜单都会拉一次,所以这一丢是**每次都丢**,永远看不到真实状态。
@Test func replacingExternalSessionsKeepsSentinel() throws {
    let daemon = DaemonStatus(running: true, pid: 1, version: nil, uptimeSecs: 10)
    let snapshot = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: daemon,
        agents: [],
        groups: [],
        externalSessions: [],
        sentinel: "on",
        waiting: [],
        menubarIcon: nil, menubarShowCounts: true
    )
    let external = ExternalSessionStatus(
        provider: .codexApp,
        id: "t1",
        title: "任务",
        cwd: "/repo",
        state: .running,
        busy: true,
        lastActiveSecs: 1,
        link: nil
    )

    let merged = snapshot.replacingExternalSessions([external])

    #expect(merged.sentinel == "on", "合并外部会话绝不能丢掉 sentinel")
    // 其余字段同样必须原样带过来 —— 这个方法的语义是「只换外部会话」。
    #expect(merged.daemon == snapshot.daemon)
    #expect(merged.externalSessions.count == 1)
}


@Test func signatureChangesWhenWaitingSetChanges() throws {
    let base = try FleetSnapshot.decode(Data(liveJSON.utf8))
    let item = WaitingItem(
        alias: "starlight", tag: "g1", name: nil, chatType: "group",
        replyMessageId: "om_1", sinceSecs: 5, sinceTextEn: "just now", link: "lark://x"
    )
    let withWaiting = FleetSnapshot(
            language: nil,
        commentTasks: [],
        commentTasksError: nil,
        daemon: base.daemon, agents: base.agents, groups: base.groups,
        externalSessions: base.externalSessions, sentinel: base.sentinel,
        waiting: [item], menubarIcon: base.menubarIcon, menubarShowCounts: true
    )
    #expect(MenuStructure.signature(fleet: base) != MenuStructure.signature(fleet: withWaiting),
            "会话已不在列表里的 waiting 自成一行,它的增减改变菜单行数")
}

@Test func waitingItemNamesAPrivateChatByTag() {
    let dm = WaitingItem(
        alias: "arcadia", tag: "p9f8", name: nil, chatType: "p2p",
        replyMessageId: "om_1", sinceSecs: 7_200, sinceTextEn: "2h ago", link: "lark://x"
    )
    #expect(dm.chatDisplayName == "DM · p9f8")
}

@Test func olderCLIWithoutWaitingKeyDecodesAsNotWaiting() throws {
    let snapshot = try FleetSnapshot.decode(Data("""
    {"daemon":{"running":true,"pid":1,"uptime_secs":5},
     "agents":[{"alias":"a","busy":false,"sessions":1,"state":"idle","idle_tmux":false,
       "chats":[{"tag":"g1","target":"soul-a-g1","busy":false,"capture":"rpc","liveness":"recent",
                 "state":"idle","idle_tmux":false}]}],
     "groups":[{"tag":"g1","busy":false,"agents":1,"state":"idle","idle_tmux":false,
       "members":[{"alias":"a","target":"soul-a-g1","busy":false,"capture":"rpc","liveness":"recent",
                   "state":"idle","idle_tmux":false}]}]}
    """.utf8))
    #expect(snapshot.agents[0].chats[0].waiting == false)
    #expect(snapshot.groups[0].members[0].waiting == false)
    #expect(snapshot.waiting.isEmpty)
}

@Test func onlyFeishuAndCodexLinksAreOpened() {
    #expect(OpenableLink.url("lark://applink.feishu.cn/client/chat/open?openChatId=oc_1") != nil)
    #expect(OpenableLink.url("feishu://applink.feishu.cn/client/chat/open?openChatId=oc_1") != nil)
    #expect(OpenableLink.url("codex://threads/01a13d7b") != nil)
    #expect(OpenableLink.url("https://example.com") == nil, "链接来自盘上文件,只放行这三种协议")
    #expect(OpenableLink.url("file:///etc/passwd") == nil)
    #expect(OpenableLink.url("not a url") == nil)
    #expect(OpenableLink.url(nil) == nil)
}

@Test func agentsWithoutSessionsShareOneLineThatOnlyCounts() throws {
    let snapshot = try FleetSnapshot.decode(Data(liveJSON.utf8))
    #expect(FleetFormat.idleAgentsLine(snapshot.agents) == "No sessions: 1 agent")
    #expect(FleetFormat.idleAgentsTooltip(snapshot.agents) == "sakura")
    let idle = try #require(snapshot.agents.first { $0.chats.isEmpty })
    let many = try (1...6).map { index -> AgentStatus in
        var object = try #require(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(idle)) as? [String: Any]
        )
        object["alias"] = "bot-\(index)"
        return try JSONDecoder().decode(
            AgentStatus.self, from: JSONSerialization.data(withJSONObject: object)
        )
    }
    #expect(FleetFormat.idleAgentsLine(many) == "No sessions: 6 agents", "名单再长,这一行宽度不变")
    #expect(FleetFormat.idleAgentsTooltip(many).contains("bot-6"))
    #expect(FleetFormat.idleAgentsLine(snapshot.agents.filter { !$0.chats.isEmpty }).isEmpty)
}

@Test func rowAgeDropsAgoToSaveWidth() {
    #expect(FleetFormat.compactAge("12m ago") == "12m")
    #expect(FleetFormat.compactAge("just now") == "now")
    #expect(FleetFormat.compactAge(">30d") == ">30d")
    #expect(FleetFormat.compactAge(nil).isEmpty)
}

/// 状态栏计数开关:旧 CLI 不带这个键时照旧显示计数。
@Test func showCountsDefaultsToOnForOlderCLI() throws {
    #expect(try FleetSnapshot.decode(Data(liveJSON.utf8)).menubarShowCounts)
    let off = try FleetSnapshot.decode(Data("""
    {"daemon":{"running":false},"agents":[],"groups":[],"menubar_show_counts":false}
    """.utf8))
    #expect(!off.menubarShowCounts)
}

@Test func commentTasksSurviveSnapshotUpdatesAndShowProcessDetails() throws {
    let data = Data("""
    {"daemon":{"running":true},"agents":[],"groups":[],"comment_tasks":[
      {"id":"task","agent":"writer","title":"Document","stage":"drafting","status":"正在起草","status_en":"Drafting","busy":true,"pid":4242,"error":null}
    ]}
    """.utf8)
    let fleet = try FleetSnapshot.decode(data)
    #expect(fleet.commentTasks.count == 1)
    #expect(fleet.commentTasks[0].line.contains("PID 4242"))
    #expect(fleet.commentTasks[0].line.contains("Drafting"))
    #expect(fleet.workingSessions == 1)
    #expect(SessionRows.build(fleet).isEmpty)
    #expect(fleet.replacingExternalSessions([]).commentTasks == fleet.commentTasks)
    #expect(MenuStructure.signature(fleet: fleet) != MenuStructure.signature(fleet: .empty))
}

@Test func commentDraftControlFollowsPersistentStage() {
    for stage in ["received", "queued", "drafting", "paused", "prepared", "draft_ready", "stopping", "closed"] {
        let task = CommentTask(id: "task", agent: "writer", title: "Title", stage: stage, status: stage, statusEn: stage, busy: false, pid: nil, error: nil)
        #expect(task.canStop == ["received", "queued", "drafting", "paused"].contains(stage))
    }
}

@Test func menuListsOnlyUnfinishedCommentTasks() {
    let daemon = DaemonStatus(running: true, pid: 1, version: nil, uptimeSecs: 10)
    func task(_ id: String, _ stage: String) -> CommentTask {
        CommentTask(id: id, agent: "writer", title: "Doc", stage: stage, status: stage, statusEn: stage, busy: false, pid: nil, error: nil)
    }
    func fleet(_ tasks: [CommentTask]) -> FleetSnapshot {
        FleetSnapshot(
            language: nil,
            commentTasks: tasks,
            commentTasksError: nil,
            daemon: daemon, agents: [], groups: [], externalSessions: [], sentinel: nil, waiting: [], menubarIcon: nil, menubarShowCounts: true)
    }
    let finished = ["replied", "closed", "failed", "expired"].map { task($0, $0) }
    let open = ["received", "queued", "drafting", "paused", "prepared", "draft_ready", "approval_failed", "sending", "stopping", "future_stage"]
        .map { task($0, $0) }

    #expect(fleet(finished + open).menuCommentTasks.map(\.id) == open.map(\.id))
    #expect(fleet(finished).menuState == .exited, "只有终态任务时不算有活")
    #expect(fleet([task("q", "queued")]).menuState == .idle)
    #expect(
        MenuStructure.signature(fleet: fleet(open)) == MenuStructure.signature(fleet: fleet(open + finished)),
        "新到的终态任务不触发重建"
    )
    #expect(
        MenuStructure.signature(fleet: fleet([task("a", "draft_ready")])) != MenuStructure.signature(fleet: fleet([task("a", "replied")])),
        "任务结束要把它那一行撤掉"
    )
}
