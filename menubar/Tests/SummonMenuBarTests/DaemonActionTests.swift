import Foundation
import Testing
@testable import SummonMenuBar

@Test func onlyOffersTheActionsThatMakeSenseRightNow() {
    // daemon 挂了:只能启动。不显示「三个里灰掉两个」。
    let whenDown = DaemonAction.allCases.filter { $0.isAvailable(daemonRunning: false) }
    #expect(whenDown == [.start])

    // daemon 在跑:启动没意义(CLI 也会拒),能做的是停或重启。
    let whenUp = DaemonAction.allCases.filter { $0.isAvailable(daemonRunning: true) }
    #expect(whenUp == [.stop, .restart])
}

@Test func destructiveActionsRequireConfirmation() {
    // start 幂等无破坏性(有单例保护),不该拦一道。stop/restart 会打断在跑的轮。
    #expect(!DaemonAction.start.needsConfirmation)
    #expect(DaemonAction.stop.needsConfirmation)
    #expect(DaemonAction.restart.needsConfirmation)
}

@Test func confirmationSpellsOutWhatItCosts() {
    // 不写泛泛的「确定吗」—— 拿在跑一轮的会话数说话,让人知道自己在付什么代价。
    let busy = DaemonConfirm.body(.stop, turnSessions: 2)
    #expect(busy.contains("2 sessions are running a turn"))
    #expect(busy.contains("interrupt"))
    #expect(busy.contains("keep their context"), "无头轮会续跑,这点必须说,否则人不敢停")

    let idle = DaemonConfirm.body(.stop, turnSessions: 0)
    #expect(idle.contains("No session is running a turn"))
    #expect(!idle.contains("interrupt them"), "没在跑就别吓唬人")

    // 这句在两种情况下都必须有:它回答「停 daemon 会不会把我的 claude 会话干掉」——不会。
    #expect(busy.contains("tmux sessions (claude) are not affected"))
    #expect(idle.contains("tmux sessions (claude) are not affected"))
}

@Test func restartConfirmationSaysRestartNotStop() {
    let body = DaemonConfirm.body(.restart, turnSessions: 1)
    #expect(body.contains("Restarting will interrupt it"))
    #expect(DaemonConfirm.confirmButton(.restart) == "Restart")
    #expect(DaemonConfirm.confirmButton(.stop) == "Stop")
}

@Test func daemonControlForwardsToTheSummonCLI() throws {
    // Menu Bar 不认识 pidfile、flock,也不发信号 —— 全部转发给既有 CLI。
    let runner = RecordingRunner(output: Data())
    let client = SummonClient(executable: URL(fileURLWithPath: "/usr/bin/true"), runner: runner)

    try client.daemonControl(.start)
    try client.daemonControl(.stop)
    try client.daemonControl(.restart)

    #expect(runner.calls == [
        ["daemon", "start"],
        ["daemon", "stop"],
        ["daemon", "restart"],
    ])
}

@Test func daemonUpDownChangesMenuStructure() throws {
    // daemon 起停会换掉控制项 —— 骨架变了,必须重建(不能只改文字)。
    let down = try FleetSnapshot.decode(Data(#"{"daemon":{"running":false},"agents":[],"groups":[]}"#.utf8))
    let up = try FleetSnapshot.decode(
        Data(#"{"daemon":{"running":true,"pid":1,"uptime_secs":2},"agents":[],"groups":[]}"#.utf8)
    )
    #expect(
        MenuStructure.signature(fleet: down) !=
            MenuStructure.signature(fleet: up)
    )
}
