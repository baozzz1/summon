import Foundation

protocol CommandRunning: Sendable {
    func run(executable: URL, arguments: [String]) throws -> Data
}

struct ProcessRunner: CommandRunning {
    func run(executable: URL, arguments: [String]) throws -> Data {
        let process = Process()
        let stdout = Pipe()
        let stderr = Pipe()
        process.executableURL = executable
        process.arguments = arguments
        process.standardOutput = stdout
        process.standardError = stderr

        do {
            try process.run()
        } catch {
            throw SummonClientError.launchFailed(error.localizedDescription)
        }
        // list --json 可能随任务数增长；先持续排空 stdout，避免 pipe 满后子进程与 waitUntilExit
        // 互相等待。CLI 错误输出有界，待进程退出后再读取即可。
        let output = stdout.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()

        let errorOutput = stderr.fileHandleForReading.readDataToEndOfFile()
        guard process.terminationStatus == 0 else {
            let message = String(data: errorOutput, encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw SummonClientError.commandFailed(
                message.flatMap { $0.isEmpty ? nil : $0 }
                    ?? L10n.text("mde8efdcc", [String(describing: (process.terminationStatus))])
            )
        }
        return output
    }
}

enum SummonClientError: LocalizedError {
    case executableNotFound
    case launchFailed(String)
    case commandFailed(String)

    var errorDescription: String? {
        switch self {
        case .executableNotFound: L10n.text("m53fcc861")
        case let .launchFailed(message): L10n.text("m4a843d68", [String(describing: (message))])
        case let .commandFailed(message): message
        }
    }
}

/// `summon menubar handled --json` 的结果词。
enum HandledOutcome: String, Decodable, Sendable {
    case cleared
    case superseded
    case absent
}

private struct HandledReply: Decodable {
    let outcome: HandledOutcome
}

/// `summon menubar end-session --json` 的结果词。
enum EndSessionOutcome: String, Decodable, Sendable {
    /// 会话已从列表里消失。
    case ended
    /// 此刻在跑一轮,CLI 没动手;要打断它得带 `--interrupt` 再来一次。
    case running
    /// 已交给 daemon,CLI 等到时限还没看到它结束。
    case pending
    /// 列表里本来就没有这条会话。
    case absent
}

private struct EndSessionReply: Decodable {
    let outcome: EndSessionOutcome
}

struct SummonClient: Sendable {
    let executable: URL
    private let runner: any CommandRunning

    init(executable: URL = Self.resolveExecutable(), runner: any CommandRunning = ProcessRunner()) {
        self.executable = executable
        self.runner = runner
    }

    /// 只读本地台账与已有价格表；独立于 daemon 与外部会话发现。
    func usage() throws -> UsageSnapshot {
        try JSONDecoder().decode(UsageSnapshot.self, from: run(["menubar", "usage"]))
    }

    /// fleet 快照。CLI 在 daemon 不在时自行降级(仍回一份合法 JSON,busy 全 false),
    /// 故这里没有「daemon 挂了」的特殊分支 —— 拿到什么就画什么。
    func fleetStatus() throws -> FleetSnapshot {
        let output = try run(["agents", "status", "--json"])
        return try FleetSnapshot.decode(output)
    }

    /// 外部发现（尤其 `claude agents --json`）比 fleet 慢，只在菜单展开/手动刷新时调用。
    func externalSessions() throws -> [ExternalSessionStatus] {
        let output = try run(["menubar", "sessions", "--json"])
        return try ExternalSessionStatus.decodeList(output)
    }

    /// 「Handled」。带上菜单渲染时那条回复的 id:期间 agent 又回了一条,CLI 就不清,
    /// 返回 `.superseded` 让菜单告诉人。
    func markHandled(_ item: WaitingItem) throws -> HandledOutcome {
        let output = try run([
            "menubar", "handled", "--agent", item.alias, "--tag", item.tag, "--reply", item.replyMessageId,
            "--json",
        ])
        return try JSONDecoder().decode(HandledReply.self, from: output).outcome
    }

    /// Exit / Archive。会阻塞到会话结束(或 CLI 等到时限),调用方必须放到后台线程。
    /// `interrupt` = 人已经确认过可以打断在跑的轮;不带时 CLI 遇到在跑的轮不动手,回 `.running`。
    func endSession(_ row: SessionRow, archive: Bool, interrupt: Bool) throws -> EndSessionOutcome {
        var arguments = ["menubar", "end-session", "--agent", row.alias, "--tag", row.tag]
        if archive { arguments.append("--archive") }
        if interrupt { arguments.append("--interrupt") }
        arguments.append("--json")
        let output = try run(arguments)
        return try JSONDecoder().decode(EndSessionReply.self, from: output).outcome
    }

    func stopComment(_ task: CommentTask) throws {
        _ = try run(["comments", "stop", task.id, "--agent", task.agent])
    }

    /// 「Dashboard…」:向 daemon 要一个一次性本机链接。只认本机回环上的 http 链接。
    func dashboardURL() throws -> URL {
        let output = try run(["dashboard", "open", "--print"])
        guard let url = DashboardLink.url(String(data: output, encoding: .utf8)) else {
            throw SummonClientError.commandFailed(L10n.text("m0e8001be"))
        }
        return url
    }

    /// 设置区的当前值。只在菜单展开 / 手动刷新时取。
    func settings() throws -> MenubarSettings {
        try MenubarSettings.decode(run(["menubar", "settings", "--json"]))
    }

    /// 改一项设置。键与值都来自 `SettingsLayout`,CLI 那边还有一道白名单。
    func setSetting(key: String, value: String) throws {
        _ = try run(["menubar", "set", key, value])
    }

    /// 起/停/重启 daemon。**难的部分全在 CLI 里**,这里只是转发:
    /// `daemon start` 有单例保护(pidfile + flock)、`setsid()` 脱离调用方(daemon 不随 Menu Bar 生死)、
    /// 阻塞等 ready 最多 5s、起不来自动回收子进程并报错(stderr 带日志路径)。
    /// 所以这三个方法都会**阻塞到有结果**为止 —— 调用方必须放到后台线程,别挡住主线程。
    func daemonControl(_ action: DaemonAction) throws {
        _ = try run(["daemon", action.command])
    }

    private func run(_ arguments: [String]) throws -> Data {
        guard FileManager.default.isExecutableFile(atPath: executable.path) else {
            throw SummonClientError.executableNotFound
        }
        return try runner.run(executable: executable, arguments: arguments)
    }

    static func resolveExecutable() -> URL {
        let environment = ProcessInfo.processInfo.environment
        if let configured = environment["SUMMON_CLI"], !configured.isEmpty {
            return URL(fileURLWithPath: configured)
        }
        let home = FileManager.default.homeDirectoryForCurrentUser
        let candidates = [
            home.appendingPathComponent(".local/bin/summon"),
            URL(fileURLWithPath: "/opt/homebrew/bin/summon"),
            URL(fileURLWithPath: "/usr/local/bin/summon"),
        ]
        return candidates.first {
            FileManager.default.isExecutableFile(atPath: $0.path)
        } ?? candidates[0]
    }
}
