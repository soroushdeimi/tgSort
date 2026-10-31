// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "tgsort-ai",
    platforms: [.macOS("26.0")],
    targets: [
        // Pure logic (protocol types, prompt building, content extraction).
        .target(name: "TgsortAICore"),
        .executableTarget(name: "tgsort-ai", dependencies: ["TgsortAICore"]),
    ]
)
