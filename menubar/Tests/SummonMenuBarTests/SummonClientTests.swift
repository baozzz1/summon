import Foundation
import Testing
@testable import SummonMenuBar

@Test func clientReadsFleetViaSummonCLI() throws {
    // Menu Bar 只经 summon CLI 的 JSON 契约:不读 config.yaml、不连 control.sock、不认 tmux。
    let runner = RecordingRunner(
        output: Data(#"{"daemon":{"running":false},"agents":[],"groups":[]}"#.utf8)
    )
    let client = SummonClient(executable: URL(fileURLWithPath: "/usr/bin/true"), runner: runner)

    let snapshot = try client.fleetStatus()

    #expect(!snapshot.daemon.running)
    #expect(runner.calls == [["agents", "status", "--json"]])
}

@Test func clientReadsExternalSessionsThroughMenubarCLIContract() throws {
    let runner = RecordingRunner(output: Data("""
    [{"provider":"codex_app","id":"t1","title":"任务","state":"running","busy":true}]
    """.utf8))
    let client = SummonClient(executable: URL(fileURLWithPath: "/usr/bin/true"), runner: runner)

    let sessions = try client.externalSessions()

    #expect(sessions.map(\.id) == ["t1"])
    #expect(runner.calls == [["menubar", "sessions", "--json"]])
}

@Test func processRunnerDrainsOutputLargerThanPipeBuffer() throws {
    let output = try ProcessRunner().run(
        executable: URL(fileURLWithPath: "/bin/sh"),
        arguments: ["-c", "i=0; while [ $i -lt 10000 ]; do printf 0123456789; i=$((i+1)); done"]
    )

    #expect(output.count == 100_000)
}

// RecordingRunner 已提到 RecordingRunner.swift（多个测试文件共用）。

@Test func markHandledPassesTheReplyItSaw() throws {
    let runner = RecordingRunner(output: Data(#"{"outcome":"superseded"}"#.utf8))
    let client = SummonClient(executable: URL(fileURLWithPath: "/bin/echo"), runner: runner)
    let item = WaitingItem(
        alias: "saturn", tag: "g1a2", name: "研发群", chatType: "group",
        replyMessageId: "om_1", sinceSecs: 60, sinceTextEn: "1m ago",
        link: "lark://applink.feishu.cn/client/chat/open?openChatId=oc_1"
    )
    let outcome = try client.markHandled(item)
    #expect(outcome == .superseded, "期间有了新回复,菜单要能告诉人")
    #expect(runner.calls.last == [
        "menubar", "handled", "--agent", "saturn", "--tag", "g1a2", "--reply", "om_1", "--json",
    ])
}

/// Exit / Archive 拼的 CLI 参数:默认退出、不打断;人确认过才带 `--interrupt`。
@Test func endSessionPassesModeAndInterruptToTheCLI() throws {
    let runner = RecordingRunner(output: Data(#"{"outcome":"running"}"#.utf8))
    let client = SummonClient(executable: URL(fileURLWithPath: "/bin/echo"), runner: runner)
    let row = SessionRow(
        alias: "saturn", tag: "g1a2", chatName: "研发群", state: .idle, age: "1m",
        busy: false, tmuxIdle: false, waiting: nil, link: nil, actions: [.exit, .archive], disabled: []
    )

    #expect(try client.endSession(row, archive: false, interrupt: false) == .running)
    _ = try client.endSession(row, archive: true, interrupt: true)
    #expect(runner.calls == [
        ["menubar", "end-session", "--agent", "saturn", "--tag", "g1a2", "--json"],
        ["menubar", "end-session", "--agent", "saturn", "--tag", "g1a2", "--archive", "--interrupt", "--json"],
    ])
}

@Test func endSessionOutcomeWordsMatchTheCLIContract() throws {
    for word in ["ended", "running", "pending", "absent"] {
        #expect(EndSessionOutcome(rawValue: word) != nil, "\(word) 是 CLI 会回的词")
    }
}

@Test func settingsReadAndWriteGoThroughTheCLI() throws {
    let runner = RecordingRunner(output: Data("""
    {"sentinel":"dry-run","show_counts":false,"launch_at_login":true,
     "cot_visible":{"value":true,"overridden_by":[]},
     "hide_thinking":{"value":false,"overridden_by":["saturn"]},
     "cot_tool_details":{"value":false,"overridden_by":[]},
     "badge_effort":{"value":true,"overridden_by":[]},
     "idle_ttl_secs":{"value":86400,"overridden_by":[]},
     "idle_retire_secs":{"value":0,"overridden_by":[]}}
    """.utf8))
    let client = SummonClient(executable: URL(fileURLWithPath: "/bin/echo"), runner: runner)
    let settings = try client.settings()
    #expect(settings.sentinel == "dry-run")
    #expect(!settings.showCounts)
    #expect(settings.hideThinking.overriddenBy == ["saturn"])
    try client.setSetting(key: "sentinel", value: "on")
    #expect(runner.calls == [["menubar", "settings", "--json"], ["menubar", "set", "sentinel", "on"]])
}
