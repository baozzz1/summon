import Foundation
import Testing
@testable import SummonMenuBar

@Test func usageGoldenDecodesAndClientOnlyReadsLocalCLI() throws {
    let url = GoldenFixture.fleetStatusURL().deletingLastPathComponent()
        .appendingPathComponent("menubar-usage.golden.json")
    let runner = RecordingRunner(output: try Data(contentsOf: url))
    let client = SummonClient(executable: URL(fileURLWithPath: "/usr/bin/true"), runner: runner)
    let snapshot = try client.usage()
    #expect(snapshot.asOfMs == 1000)
    #expect(snapshot.retentionDays == 90)
    #expect(snapshot.pricesAsOfMs == nil)
    #expect(snapshot.windows.map(\.key) == ["24h", "7d", "30d", "all"])
    #expect(snapshot.windows.allSatisfy { $0.amount == "No usage data" })
    #expect(runner.calls == [["menubar", "usage"]])
    #expect(snapshot.tooltip.contains("after completion"))
    #expect(snapshot.tooltip.contains("90-day retention"))
}

private func usageWindow(cost: Double, tokens: UInt64 = 100, unpriced: UInt64? = nil, missing: Int = 0) throws -> UsageWindow {
    var spend: [String: Any] = ["cost": ["total": cost], "tokens": ["input": tokens]]
    if let unpriced { spend["unpriced"] = ["tokens": unpriced] }
    let data = try JSONSerialization.data(withJSONObject: [
        "key": "24h", "spend": spend, "missing_usage_turns": missing,
    ])
    return try JSONDecoder().decode(UsageWindow.self, from: data)
}

@Test func usageAmountsKeepUnknownAndSmallCostsVisible() throws {
    #expect(try usageWindow(cost: 12.34).amount == "$12.34")
    #expect(try usageWindow(cost: 0).amount == "$0.00")
    #expect(try usageWindow(cost: 0.001).amount == "<$0.01")
    #expect(try usageWindow(cost: 0, unpriced: 100).amount == "Unpriced")
    #expect(try usageWindow(cost: 1.23, unpriced: 20).amount == "$1.23+")
    #expect(try usageWindow(cost: 1.23, missing: 2).amount == "$1.23+")
    #expect(try usageWindow(cost: 1.23, missing: 2).tooltip.contains("2 turns"))
}

@Test func malformedUsageFailsInsteadOfReportingZero() {
    let runner = RecordingRunner(output: Data(#"{"error":"cannot read"}"#.utf8))
    let client = SummonClient(executable: URL(fileURLWithPath: "/usr/bin/true"), runner: runner)
    #expect(throws: (any Error).self) { try client.usage() }
}

@Test func pricedUsageGoldenDecodesEveryConsumedField() throws {
    let url = GoldenFixture.fleetStatusURL().deletingLastPathComponent()
        .appendingPathComponent("menubar-usage-priced.golden.json")
    let snapshot = try JSONDecoder().decode(UsageSnapshot.self, from: Data(contentsOf: url))
    #expect(snapshot.asOfMs == 1000)
    #expect(snapshot.pricesAsOfMs == 100)
    for window in snapshot.windows {
        #expect(window.amount == "$1.00")
        #expect(window.missingUsageTurns == 0)
        #expect(window.spend?.tokens["output"] == 100)
        #expect(window.spend?.cost.total == 1)
        #expect(window.spend?.unpriced == nil)
    }
}

@Test func usageFootnoteExplainsPartialAmounts() throws {
    let url = GoldenFixture.fleetStatusURL().deletingLastPathComponent()
        .appendingPathComponent("menubar-usage-priced.golden.json")
    var object = try #require(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
    let complete = try JSONDecoder().decode(UsageSnapshot.self, from: JSONSerialization.data(withJSONObject: object))
    #expect(!complete.note.hasPrefix("+"))
    var windows = try #require(object["windows"] as? [[String: Any]])
    windows[0]["missing_usage_turns"] = 2
    object["windows"] = windows
    let partial = try JSONDecoder().decode(UsageSnapshot.self, from: JSONSerialization.data(withJSONObject: object))
    #expect(partial.windows[0].amount == "$1.00+")
    #expect(partial.note == "+ Amounts exclude missing or unpriced usage")
}

@Test func tokenUsageAddsEveryRecordedCategoryAndIncludesUnpricedTokens() throws {
    let data = Data(#"{"key":"24h","missing_usage_turns":0,"spend":{"tokens":{"input":1000,"output":2000,"cache_read":3000,"cache_write":4000},"cost":{"total":1},"unpriced":{"tokens":3000}}}"#.utf8)
    let window = try JSONDecoder().decode(UsageWindow.self, from: data)
    #expect(window.totalTokens == 10_000)
    #expect(window.tokenUsage == "10K tokens")
    #expect(window.tooltip.contains("10,000 tokens recorded"))
    #expect(!window.tokenUsage.contains("+"))
}

@Test func tokenUsageCompactsCountsAndPreservesMissingData() throws {
    #expect(try usageWindow(cost: 0, tokens: 0).tokenUsage == "0 tokens")
    #expect(try usageWindow(cost: 1, tokens: 999).tokenUsage == "999 tokens")
    #expect(try usageWindow(cost: 1, tokens: 1234).tokenUsage == "1.2K tokens")
    #expect(try usageWindow(cost: 1, tokens: 12_345_678).tokenUsage == "12.3M tokens")
    #expect(try usageWindow(cost: 1, tokens: 1_234_567_890).tokenUsage == "1.2B tokens")
    #expect(try usageWindow(cost: 1, tokens: 12_345_678, missing: 2).tokenUsage == "12.3M+ tokens")
    #expect(try usageWindow(cost: 0, tokens: 100, unpriced: 100).tokenUsage == "100 tokens")
    let absent = try JSONDecoder().decode(UsageWindow.self, from: Data(#"{"key":"24h","missing_usage_turns":1,"spend":null}"#.utf8))
    #expect(absent.totalTokens == nil)
    #expect(absent.tokenUsage == "—")
}
