import Foundation
import Testing
@testable import SummonMenuBar

/// 「Dashboard…」只把本机回环上的 http 链接交给系统打开:链接是 CLI 打出来的一行字,
/// 别的主机、别的协议、没写端口的一律不开。
@Test func dashboardItemOpensOnlyLoopbackHttp() {
    for raw in [
        "http://127.0.0.1:7767/local/0123abcd",
        "http://localhost:7767/local/0123abcd",
        "http://[::1]:7767/local/0123abcd",
        "  http://127.0.0.1:7767/local/0123abcd\n",
    ] {
        #expect(DashboardLink.url(raw) != nil, "\(raw)")
    }
    for raw in [
        "https://127.0.0.1:7767/local/x",
        "http://10.1.2.3:7767/local/x",
        "http://127.0.0.1.evil.example:7767/local/x",
        "http://127.0.0.1:7767@evil.example/local/x",
        "http://user@127.0.0.1:7767/local/x",
        "http://127.0.0.1/local/x",
        "lark://applink.feishu.cn/client/chat/open?openChatId=oc_1",
        "file:///etc/passwd",
        "",
        "not a url",
    ] {
        #expect(DashboardLink.url(raw) == nil, "\(raw)")
    }
    #expect(DashboardLink.url(nil) == nil)
}

/// daemon 不在时本机码签不出来,这一行不显示。
@Test func dashboardItemShowsOnlyWhileDaemonRuns() {
    #expect(DashboardLink.offered(daemonRunning: true))
    #expect(!DashboardLink.offered(daemonRunning: false))
}

/// 只经 CLI 契约:`summon dashboard open --print`,拿回那一行链接。
@Test func dashboardURLComesFromTheCLI() throws {
    let runner = RecordingRunner(output: Data("http://127.0.0.1:7767/local/0123abcd\n".utf8))
    let client = SummonClient(executable: URL(fileURLWithPath: "/bin/echo"), runner: runner)
    let url = try client.dashboardURL()
    #expect(url.absoluteString == "http://127.0.0.1:7767/local/0123abcd")
    #expect(runner.calls == [["dashboard", "open", "--print"]])
}

@Test func aNonLoopbackDashboardURLIsRefused() {
    let runner = RecordingRunner(output: Data("http://10.1.2.3:7767/local/0123abcd\n".utf8))
    let client = SummonClient(executable: URL(fileURLWithPath: "/bin/echo"), runner: runner)
    #expect(throws: SummonClientError.self) { try client.dashboardURL() }
}
