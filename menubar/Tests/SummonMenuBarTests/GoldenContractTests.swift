import Foundation
import Testing
@testable import SummonMenuBar

/// 跨语言 JSON 契约的**共享** golden:Rust(`summon_core::observe::fleet` 的
/// `fleet_snapshot_matches_cross_language_golden`)、Swift(本文件)
/// 解的是同一个 `tests/fixtures/fleet-status.golden.json`。
///
/// 为什么必须共享而不是各写各的内联 JSON:Swift `Codable` 和 JS 取值对未知 key 都是**静默忽略**
/// —— `last_active_secs` 一改名,Menu Bar 全变空白,三边测试却仍然全绿。共享 fixture 把这种
/// 静默降级变成 Rust 侧先红、消费者跟着红。
enum GoldenFixture {
    /// 本文件在 `<repo>/menubar/Tests/SummonMenuBarTests/`,往上三层就是仓库根。
    /// 不用 Bundle 资源:fixture 归三种语言共有,不属于 SwiftPM target。
    static func fleetStatusURL(_ file: StaticString = #filePath) -> URL {
        URL(fileURLWithPath: "\(file)")
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("tests/fixtures/fleet-status.golden.json")
    }
}

@Test func goldenFleetSnapshotDecodesEveryContractField() throws {
    let data = try Data(contentsOf: GoldenFixture.fleetStatusURL())
    let snapshot = try FleetSnapshot.decode(data)

    #expect(snapshot.daemon.running)
    #expect(snapshot.daemon.pid == 4242)
    #expect(snapshot.daemon.uptimeSecs == 269_400)
    #expect(snapshot.sentinel == "dry-run")
    #expect(snapshot.agents.count == 4)
    #expect(snapshot.groups.count == 6)

    // 改名后 Codable 会把可选字段解成 nil、把整体解成空表 —— 逐个钉住,别让它悄悄降级。
    let starlight = try #require(snapshot.agents.first)
    #expect(starlight.alias == "starlight")
    #expect(starlight.busy)
    #expect(starlight.sessions == 2)
    #expect(starlight.lastActiveSecs == 3)
    #expect(starlight.lastActiveTextEn == "just now")

    let chat = try #require(starlight.chats.first)
    #expect(chat.tag == "g1a2b3c4")
    #expect(chat.target == "soul-starlight-g1a2b3c4")
    #expect(chat.name == "研发群")
    #expect(chat.chatType == "group")
    #expect(chat.capture == "transcript")
    #expect(chat.liveness == "live")
    #expect(chat.busy)

    let group = try #require(snapshot.groups.first)
    #expect(group.tag == "g1a2b3c4")
    #expect(group.name == "研发群")
    #expect(group.chatType == "group")
    #expect(group.agents == 2)
    #expect(group.busy)
    #expect(group.lastActiveTextEn == "just now")
    #expect(group.chatId == "oc_g1a2b3c4")
    let member = try #require(group.members.last)
    #expect(member.alias == "saturn")
    #expect(member.target == "soul-saturn-g1a2b3c4")
    // 2026-08-20 起捕获模式只剩两种:transcript 与 rpc(Subprocess / CliSession 已删)。
    #expect(member.capture == "rpc")
    #expect(member.liveness == "recent")
    #expect(member.lastActiveSecs == 45)
    #expect(member.lastActiveTextEn == "45s ago")

    // 从没活跃过的 agent:两个 last-active 字段一起缺席。
    let sakura = try #require(snapshot.agents.last)
    #expect(sakura.alias == "sakura")
    #expect(sakura.lastActiveSecs == nil)
    #expect(sakura.lastActiveTextEn == nil)
    #expect(sakura.chats.isEmpty)
}

/// `FleetFormat.sinceText` 是回退路径(外部会话只有秒数,没有 `last_active_text_en`)。
/// 它必须与 Rust 渲染出的文案逐字相同 —— 拿 golden 里成对的 secs/text 当判据,不再手抄期望值。
@Test func fallbackSinceTextMatchesRustRenderedText() throws {
    let data = try Data(contentsOf: GoldenFixture.fleetStatusURL())
    let snapshot = try FleetSnapshot.decode(data)

    var pairs: [(UInt64, String)] = []
    for agent in snapshot.agents {
        if let secs = agent.lastActiveSecs, let text = agent.lastActiveTextEn {
            pairs.append((secs, text))
        }
        for chat in agent.chats {
            if let secs = chat.lastActiveSecs, let text = chat.lastActiveTextEn {
                pairs.append((secs, text))
            }
        }
    }
    for group in snapshot.groups {
        if let secs = group.lastActiveSecs, let text = group.lastActiveTextEn {
            pairs.append((secs, text))
        }
        for member in group.members {
            if let secs = member.lastActiveSecs, let text = member.lastActiveTextEn {
                pairs.append((secs, text))
            }
        }
    }

    // just now / s / h / d / >30d 各档都要被 fixture 覆盖到,否则这条测试形同虚设。
    #expect(Set(pairs.map(\.1)).count >= 5)
    for (secs, text) in pairs {
        #expect(FleetFormat.sinceText(secs) == text, "secs=\(secs) 的本地分桶与 Rust 文案不一致")
    }
}

@Test func goldenCarriesWaitingAndLinks() throws {
    let snapshot = try FleetSnapshot.decode(Data(contentsOf: GoldenFixture.fleetStatusURL()))
    #expect(snapshot.menubarIcon == "xyz")
    #expect(snapshot.waiting.count == 1)
    let item = try #require(snapshot.waiting.first)
    #expect(item.alias == "saturn")
    #expect(item.tag == "g1a2b3c4")
    #expect(item.replyMessageId == "om_golden_reply")
    #expect(item.sinceTextEn == "12m ago")
    #expect(item.link == "lark://applink.feishu.cn/client/chat/open?openChatId=oc_golden&position=1485")
    let group = try #require(snapshot.groups.first { $0.tag == "g1a2b3c4" })
    #expect(group.link?.hasPrefix("lark://applink.feishu.cn/client/chat/open?openChatId=") == true)
    #expect(group.members.first { $0.alias == "saturn" }?.waiting == true)
    #expect(group.members.first { $0.alias == "starlight" }?.waiting == false)
    // 会话行:在跑的归 working,没在跑又 waiting 的归 waiting(时间从那条回复算起)。
    let rows = SessionRows.build(snapshot).filter { $0.tag == "g1a2b3c4" }
    #expect(rows.map(\.summary) == ["研发群 · saturn · waiting · 12m", "研发群 · starlight · working · now"])
    #expect(snapshot.workingSessions == 1)
}

@Test func goldenCarriesDocumentCommentTasks() throws {
    let fleet = try FleetSnapshot.decode(Data(contentsOf: GoldenFixture.fleetStatusURL()))
    #expect(fleet.commentTasks.count == 1)
    #expect(fleet.commentTasks[0].stage == "draft_ready")
    #expect(fleet.commentTasks[0].statusEn == "Awaiting approval")
}
