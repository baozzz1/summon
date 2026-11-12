// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "SummonMenuBar",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "summon-menubar", targets: ["SummonMenuBar"]),
    ],
    targets: [
        .executableTarget(name: "SummonMenuBar"),
        .testTarget(name: "SummonMenuBarTests", dependencies: ["SummonMenuBar"]),
    ]
)
