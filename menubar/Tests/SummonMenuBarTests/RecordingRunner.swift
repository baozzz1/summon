import Foundation
@testable import SummonMenuBar

/// 记录 summon CLI 被怎么调用的假 runner。多个测试文件共用 —— Menu Bar 的全部对外行为
/// 都是「spawn 了哪条 summon 命令」,所以这是最要紧的断言面。
final class RecordingRunner: CommandRunning, @unchecked Sendable {
    private let output: Data
    private(set) var calls: [[String]] = []

    init(output: Data) {
        self.output = output
    }

    func run(executable: URL, arguments: [String]) throws -> Data {
        calls.append(arguments)
        return output
    }
}
