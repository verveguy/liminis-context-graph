// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "local-inference",
    platforms: [.macOS(.v26)],
    dependencies: [
        .package(url: "https://github.com/hummingbird-project/hummingbird", from: "2.0.0"),
        .package(url: "https://github.com/huggingface/swift-transformers", from: "0.1.14"),
    ],
    targets: [
        .executableTarget(
            name: "LocalInference",
            dependencies: [
                .product(name: "Hummingbird", package: "hummingbird"),
                .product(name: "Transformers", package: "swift-transformers"),
            ],
            swiftSettings: [
                .enableUpcomingFeature("ExistentialAny"),
            ]
        ),
        .testTarget(
            name: "LocalInferenceTests",
            dependencies: [
                .target(name: "LocalInference"),
                .product(name: "Hummingbird", package: "hummingbird"),
                .product(name: "HummingbirdTesting", package: "hummingbird"),
                .product(name: "Transformers", package: "swift-transformers"),
            ],
            resources: [
                .copy("Fixtures"),
            ],
            swiftSettings: [
                .enableUpcomingFeature("ExistentialAny"),
            ]
        ),
    ]
)
