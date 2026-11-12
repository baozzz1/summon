import AppKit
import Testing
@testable import SummonMenuBar

@Test func everyIconLoadsAsMultiScaleTemplate() {
    let keys = ["working", "waiting"] + [
        "xyz", "millennium-eye", "link", "synchro", "card", "fusion", "ring-star", "hex-star",
    ].map { "summon-\($0)" }
    for key in keys {
        let image = StatusIcons.image(key)
        #expect(image != nil, "缺图标 \(key)")
        #expect(image?.isTemplate == true, "\(key) 必须是模板图,才会随明暗着色")
        #expect(image?.representations.count == 3, "\(key) 要有 1x/2x/3x")
    }
    #expect(StatusIcons.image("summon-xyz")?.size == NSSize(width: 17, height: 17))
    #expect(StatusIcons.image("working")?.size == NSSize(width: 16, height: 16))
}

@Test func unknownSummonIconFallsBackToDefault() {
    #expect(StatusIcons.summon(named: "no-such-icon") != nil)
    #expect(StatusIcons.summon(named: nil) != nil)
}

@Test func zeroSegmentsAreHidden() {
    #expect(StatusBarImage.segments(working: 0, waiting: 0).isEmpty)
    #expect(StatusBarImage.segments(working: 3, waiting: 0).map(\.icon) == ["working"])
    #expect(StatusBarImage.segments(working: 0, waiting: 2).map(\.icon) == ["waiting"])
    #expect(StatusBarImage.segments(working: 3, waiting: 2).map(\.count) == [3, 2])
}

@Test func composedImageIsTemplateAndGrowsWithSegments() {
    let summon = StatusIcons.summon(named: "xyz")
    let bare = StatusBarImage.compose(summon: summon, working: 0, waiting: 0)
    let both = StatusBarImage.compose(summon: summon, working: 3, waiting: 12)
    #expect(bare.isTemplate && both.isTemplate)
    #expect(bare.size.width == 17, "都为 0 时只剩 summon 图标")
    #expect(both.size.width > bare.size.width + 32)
}

@Test func accessibilityLabelCarriesTheWords() {
    #expect(StatusBarImage.accessibilityLabel(working: 3, waiting: 2, daemonRunning: true, leaked: false)
        == "summon · 3 working · 2 waiting")
    #expect(StatusBarImage.accessibilityLabel(working: 0, waiting: 1, daemonRunning: false, leaked: false)
        == "summon · daemon not running · 1 waiting")
    #expect(StatusBarImage.accessibilityLabel(working: 0, waiting: 0, daemonRunning: true, leaked: true)
        == "summon · 0 working · 0 waiting · a session closed but its process is still alive")
}
