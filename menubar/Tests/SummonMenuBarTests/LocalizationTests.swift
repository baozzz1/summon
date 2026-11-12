import Foundation
import Testing
@testable import SummonMenuBar

@Test func localeCatalogsCoverTheSameKeysAndFormatArguments() throws {
    let pattern = try NSRegularExpression(pattern: #"\{\d+\}|%[@sdv]"#)
    func tokens(_ value: String) -> [String] {
        let text = value as NSString
        return pattern.matches(in: value, range: NSRange(location: 0, length: text.length)).map { text.substring(with: $0.range) }.sorted()
    }
    #expect(Set(LocalizationResources.english.keys) == Set(LocalizationResources.chinese.keys))
    for (key, english) in LocalizationResources.english {
        let chinese = try #require(LocalizationResources.chinese[key])
        #expect(!english.isEmpty && !chinese.isEmpty)
        #expect(tokens(english) == tokens(chinese))
        #expect(english.range(of: #"[\u3400-\u9fff]"#, options: .regularExpression) == nil)
    }
}

@Test func localeStateSwitchesWithoutChangingArguments() {
    let locale = LocaleState()
    #expect(locale.text("md17d84a6") == "Exit")
    #expect(locale.setLanguage("zh-CN"))
    #expect(locale.text("md17d84a6") == "退出")
    #expect(locale.setLanguage("en"))
    #expect(locale.text("m337406f7", ["{1}", "3"]) == "{1}3 tokens")
}
