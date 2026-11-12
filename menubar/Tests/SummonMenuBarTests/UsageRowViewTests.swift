import AppKit
import Testing
@testable import SummonMenuBar

@MainActor
private func renderUsageViews(_ views: [NSView], appearance: NSAppearance) throws -> NSBitmapImageRep {
    let width = Int(views.map { $0.bounds.width }.max() ?? UsageRowView.minimumWidth)
    let height = Int(views.reduce(0) { $0 + $1.bounds.height })
    let bitmap = try #require(NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    ))
    let context = try #require(NSGraphicsContext(bitmapImageRep: bitmap))
    NSGraphicsContext.saveGraphicsState()
    defer { NSGraphicsContext.restoreGraphicsState() }
    NSGraphicsContext.current = context
    appearance.performAsCurrentDrawingAppearance {
        NSColor.windowBackgroundColor.setFill()
        NSBezierPath(rect: NSRect(x: 0, y: 0, width: width, height: height)).fill()
        var top = CGFloat(height)
        for view in views {
            top -= view.bounds.height
            NSGraphicsContext.saveGraphicsState()
            let transform = NSAffineTransform()
            transform.translateX(by: 0, yBy: top)
            transform.concat()
            view.draw(view.bounds)
            NSGraphicsContext.restoreGraphicsState()
        }
    }
    return bitmap
}

private func luminance(_ color: NSColor) throws -> Double {
    let rgb = try #require(color.usingColorSpace(.deviceRGB))
    func linear(_ value: CGFloat) -> Double {
        let value = Double(value)
        return value <= 0.04045 ? value / 12.92 : pow((value + 0.055) / 1.055, 2.4)
    }
    return 0.2126 * linear(rgb.redComponent) + 0.7152 * linear(rgb.greenComponent) + 0.0722 * linear(rgb.blueComponent)
}

private func legiblePixels(_ bitmap: NSBitmapImageRep, fromX: Int, toX: Int) throws -> [(Int, Int)] {
    let background = try luminance(#require(bitmap.colorAt(x: 0, y: 0)))
    var pixels: [(Int, Int)] = []
    for x in fromX..<toX {
        for y in 0..<bitmap.pixelsHigh {
            let value = try luminance(#require(bitmap.colorAt(x: x, y: y)))
            let contrast = (max(value, background) + 0.05) / (min(value, background) + 0.05)
            if contrast >= 4.5 { pixels.append((x, y)) }
        }
    }
    return pixels
}

@MainActor
@Test func usageTextHasReadableContrastAndAlignedAmountsInBothThemes() throws {
    for name in [NSAppearance.Name.aqua, .darkAqua] {
        let appearance = try #require(NSAppearance(named: name))
        let short = UsageRowView(label: "Past 24 hours", amount: "$12.34+", tokens: "12.3M tokens")
        let long = UsageRowView(label: "Past 24 hours", amount: "$12,345.34+", tokens: "987.6M tokens")
        let shortImage = try renderUsageViews([short], appearance: appearance)
        let longImage = try renderUsageViews([long], appearance: appearance)
        #expect(try legiblePixels(shortImage, fromX: 16, toX: 114).count > 40)
        let shortAmount = try legiblePixels(shortImage, fromX: 160, toX: shortImage.pixelsWide)
        let longAmount = try legiblePixels(longImage, fromX: 160, toX: longImage.pixelsWide)
        #expect(shortAmount.count > 40)
        #expect(longAmount.count > 40)
        #expect(shortAmount.map(\.0).max() == longAmount.map(\.0).max())
        let tokenEnd = shortImage.pixelsWide - 14 - 112 - 8
        let shortTokens = try legiblePixels(shortImage, fromX: 120, toX: tokenEnd)
        let longTokens = try legiblePixels(longImage, fromX: 120, toX: tokenEnd)
        #expect(shortTokens.count > 40)
        #expect(longTokens.count > 40)
        #expect(shortTokens.map(\.0).max() == longTokens.map(\.0).max())

        if let path = ProcessInfo.processInfo.environment["SUMMON_USAGE_PREVIEW_DIR"] {
            let rows = zip(UsageWindow.labels, ["$12.34+", "$56.78+", "$123.45+", "$234.56+"]).map { entry, amount in
                UsageRowView(label: entry.1, amount: amount, tokens: ["24h": "12.3M+ tokens", "7d": "56.7M+ tokens", "30d": "123.4M+ tokens", "all": "234.5M+ tokens"][entry.0]!)
            }
            let header = SectionHeaderView(title: "Estimated cost · USD")
            header.frame.size.width = UsageRowView.minimumWidth
            let footer = UsageFooterView()
            footer.configure(note: "+ Amounts exclude missing or unpriced usage", updated: "As of 17:47:58 · refresh to update", tooltip: nil)
            let panel = try renderUsageViews([header] + rows + [footer], appearance: appearance)
            let png = try #require(panel.representation(using: .png, properties: [:]))
            let file = URL(fileURLWithPath: path).appendingPathComponent("summon-usage-\(name == .aqua ? "light" : "dark").png")
            try png.write(to: file)
        }
    }
}

@MainActor
@Test func usageRowsKeepInPlaceUpdatesAndAccessibleValues() {
    let row = UsageRowView(label: "Past 24 hours", amount: "Loading…", tokens: "—")
    let initial = row.frame.size
    row.configure(amount: "$12.34+", tokens: "12.3M+ tokens", tooltip: "2 turns have no recorded token usage")
    #expect(row.frame.size == initial)
    #expect(row.accessibilityLabel() == "Past 24 hours: 12.3M+ tokens, $12.34+")
    #expect(row.toolTip?.contains("2 turns") == true)
    row.configure(amount: "Unavailable", tokens: "—", tooltip: "Cannot read local usage")
    #expect(row.amount == "Unavailable")
    #expect(row.accessibilityLabel() == "Past 24 hours: —, Unavailable")
}

@MainActor
@Test func compactUsageRowsPreserveEveryColumnInBothThemes() throws {
    for name in [NSAppearance.Name.aqua, .darkAqua] {
        let appearance = try #require(NSAppearance(named: name))
        for label in UsageWindow.labels.map(\.1) {
            let narrow = UsageRowView(label: label, amount: "$12,345,678.90+", tokens: "987.6B+ tokens")
            #expect(narrow.frame.width <= 360)
            let wide = UsageRowView(label: label, amount: "$12,345,678.90+", tokens: "987.6B+ tokens")
            wide.frame.size.width = 480
            let narrowImage = try renderUsageViews([narrow], appearance: appearance)
            let wideImage = try renderUsageViews([wide], appearance: appearance)
            #expect(try legiblePixels(narrowImage, fromX: 16, toX: 120).count == legiblePixels(wideImage, fromX: 16, toX: 120).count)
            #expect(try legiblePixels(narrowImage, fromX: 120, toX: 226).count == legiblePixels(wideImage, fromX: 240, toX: 346).count)
            #expect(try legiblePixels(narrowImage, fromX: 234, toX: 360).count == legiblePixels(wideImage, fromX: 354, toX: 480).count)
        }
    }
}
