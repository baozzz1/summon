import AppKit

/// 状态栏用的单色模板图。像素来自 `IconData`(由 `menubar/Icons/build-icons.sh` 从 SVG 生成)。
enum StatusIcons {
    static let defaultSummon = "xyz"

    static func image(_ key: String) -> NSImage? {
        guard let entry = IconData.icons[key] else { return nil }
        let image = NSImage(size: NSSize(width: entry.points, height: entry.points))
        for scale in entry.pngs.keys.sorted() {
            guard let encoded = entry.pngs[scale],
                  let data = Data(base64Encoded: encoded),
                  let rep = NSBitmapImageRep(data: data)
            else { continue }
            rep.size = image.size
            image.addRepresentation(rep)
        }
        guard !image.representations.isEmpty else { return nil }
        image.isTemplate = true
        return image
    }

    /// 认不出的名字(手改错、旧 CLI 没带)按默认图标画,状态栏不能空。
    static func summon(named name: String?) -> NSImage? {
        name.flatMap { image("summon-\($0)") } ?? image("summon-\(defaultSummon)")
    }
}

/// 状态栏内容:summon 图标 + working / waiting 两段「图标 + 数字」,0 的段不画。
///
/// 合成**一张**模板图交给系统着色:图标与文字分开放(image + title)时,二者在高亮态和深浅色
/// 切换下着色并不一致。状态文字通过辅助功能标签提供。
enum StatusBarImage {
    struct Segment: Equatable {
        let icon: String
        let count: Int
    }

    static let height: CGFloat = 18
    private static let segmentGap: CGFloat = 6
    private static let digitGap: CGFloat = 2

    static func segments(working: Int, waiting: Int) -> [Segment] {
        [Segment(icon: "working", count: working), Segment(icon: "waiting", count: waiting)]
            .filter { $0.count > 0 }
    }

    static func compose(summon: NSImage?, working: Int, waiting: Int) -> NSImage {
        let attributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedDigitSystemFont(ofSize: 13, weight: .medium),
            .foregroundColor: NSColor.black,
        ]
        let parts = segments(working: working, waiting: waiting).map {
            (StatusIcons.image($0.icon), NSAttributedString(string: "\($0.count)", attributes: attributes))
        }
        var width = summon?.size.width ?? 0
        for (icon, text) in parts {
            width += segmentGap + (icon?.size.width ?? 0) + digitGap + ceil(text.size().width)
        }
        let height = Self.height
        let image = NSImage(size: NSSize(width: max(width, 1), height: height), flipped: false) { _ in
            var x: CGFloat = 0
            if let summon {
                summon.draw(in: NSRect(
                    x: x, y: (height - summon.size.height) / 2,
                    width: summon.size.width, height: summon.size.height
                ))
                x += summon.size.width
            }
            for (icon, text) in parts {
                x += segmentGap
                if let icon {
                    icon.draw(in: NSRect(
                        x: x, y: (height - icon.size.height) / 2,
                        width: icon.size.width, height: icon.size.height
                    ))
                    x += icon.size.width
                }
                x += digitGap
                let size = text.size()
                text.draw(at: NSPoint(x: x, y: (height - size.height) / 2))
                x += ceil(size.width)
            }
            return true
        }
        image.isTemplate = true
        return image
    }

    static func accessibilityLabel(working: Int, waiting: Int, daemonRunning: Bool, leaked: Bool) -> String {
        var parts = ["summon"]
        if daemonRunning {
            parts.append(L10n.text("m56f7e401", [String(describing: (working))]))
        } else {
            parts.append(L10n.text("maf283f1e"))
        }
        if daemonRunning || waiting > 0 {
            parts.append(L10n.text("m15f51b52", [String(describing: (waiting))]))
        }
        if leaked {
            parts.append(L10n.text("md3d9e974"))
        }
        return parts.joined(separator: " · ")
    }
}
