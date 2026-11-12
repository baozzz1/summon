import Foundation

final class LocaleState: @unchecked Sendable {
    private let lock = NSLock()
    private var selected = "en"

    var language: String {
        lock.lock()
        defer { lock.unlock() }
        return selected
    }

    @discardableResult
    func setLanguage(_ code: String?) -> Bool {
        let next = code?.lowercased().hasPrefix("zh") == true ? "zh-CN" : "en"
        lock.lock()
        defer { lock.unlock() }
        let changed = selected != next
        selected = next
        return changed
    }

    func text(_ key: String, _ args: [String] = []) -> String {
        Self.render(key, language: language, args: args)
    }

    private static let placeholders = try! NSRegularExpression(pattern: #"\{(\d+)\}"#)

    static func render(_ key: String, language: String, args: [String]) -> String {
        let catalog = language == "zh-CN" ? LocalizationResources.chinese : LocalizationResources.english
        let source = (catalog[key] ?? LocalizationResources.english[key] ?? "[\(key)]") as NSString
        var output = ""
        var cursor = 0
        for match in placeholders.matches(in: source as String, range: NSRange(location: 0, length: source.length)) {
            output += source.substring(with: NSRange(location: cursor, length: match.range.location - cursor))
            let index = Int(source.substring(with: match.range(at: 1)))!
            output += args.indices.contains(index) ? args[index] : source.substring(with: match.range)
            cursor = NSMaxRange(match.range)
        }
        output += source.substring(from: cursor)
        return output
    }
}

enum L10n {
    static let state = LocaleState()
    static func text(_ key: String, _ args: [String] = []) -> String { state.text(key, args) }
}
