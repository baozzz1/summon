import Foundation

struct UsageSnapshot: Decodable, Sendable {
    let asOfMs: Int64
    let retentionDays: Int
    let pricesAsOfMs: Int64?
    let windows: [UsageWindow]

    enum CodingKeys: String, CodingKey {
        case asOfMs = "as_of_ms"
        case retentionDays = "retention_days"
        case pricesAsOfMs = "prices_as_of_ms"
        case windows
    }

    var note: String {
        windows.contains { $0.spend?.unpriced != nil || $0.missingUsageTurns > 0 }
            ? L10n.text("m30a3fae9")
            : L10n.text("m107d3156")
    }

    var detail: String {
        let at = Date(timeIntervalSince1970: Double(asOfMs) / 1000)
            .formatted(date: .omitted, time: .standard)
        return L10n.text("m265f1340", [String(describing: (at))])
    }

    var tooltip: String {
        var text = L10n.text("mb9fd5146")
        text += L10n.text("me73a9e87", [String(describing: (retentionDays))])
        if let pricesAsOfMs {
            let date = Date(timeIntervalSince1970: Double(pricesAsOfMs) / 1000)
                .formatted(date: .abbreviated, time: .shortened)
            text += L10n.text("m0285bf03", [String(describing: (date))])
        } else {
            text += L10n.text("m3f7dacb8")
        }
        return text
    }
}

struct UsageWindow: Decodable, Sendable {
    let key: String
    let spend: UsageSpend?
    let missingUsageTurns: Int

    enum CodingKeys: String, CodingKey {
        case key, spend
        case missingUsageTurns = "missing_usage_turns"
    }

    static let labels = [("24h", L10n.text("m324f17e9")), ("7d", L10n.text("m8a28e5fd")),
                         ("30d", L10n.text("mea8721e4")), ("all", L10n.text("md81ae686"))]

    var totalTokens: UInt64? {
        spend.map { $0.tokens.values.reduce(0, +) }
    }

    var tokenUsage: String {
        guard let totalTokens else { return "—" }
        let count = totalTokens.formatted(
            .number.notation(.compactName).precision(.fractionLength(0...1)).locale(Locale(identifier: "en_US"))
        )
        return L10n.text("m337406f7", [String(describing: (count)), String(describing: (missingUsageTurns > 0 ? "+" : ""))])
    }

    var amount: String {
        guard let spend else { return L10n.text("m638d55f9") }
        let tokens = spend.tokens.values.reduce(0, +)
        let unpriced = spend.unpriced?.tokens ?? 0
        if spend.unpriced != nil, unpriced >= tokens { return L10n.text("mc05dd6af") }
        let cost = spend.cost.total
        let formatted: String
        if cost > 0, cost < 0.01 {
            formatted = "<$0.01"
        } else {
            formatted = cost.formatted(.currency(code: "USD").locale(Locale(identifier: "en_US")))
        }
        let incomplete = spend.unpriced != nil || missingUsageTurns > 0
        return incomplete ? "\(formatted)+" : formatted
    }

    var tooltip: String {
        var text = L10n.text("mab6700b4")
        if let totalTokens {
            let count = totalTokens.formatted(.number.locale(Locale(identifier: "en_US")))
            text += L10n.text("m9b5f5916", [String(describing: (count))])
        }
        if let unpriced = spend?.unpriced {
            text += L10n.text("m177f73f2", [String(describing: (unpriced.tokens))])
        }
        if missingUsageTurns > 0 {
            text += L10n.text("m70e213a4", [String(describing: (missingUsageTurns))])
        }
        return text
    }
}

struct UsageSpend: Decodable, Sendable {
    struct Cost: Decodable, Sendable { let total: Double }
    struct Unpriced: Decodable, Sendable { let tokens: UInt64 }
    let tokens: [String: UInt64]
    let cost: Cost
    let unpriced: Unpriced?
}
