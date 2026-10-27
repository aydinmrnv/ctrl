// swift-tools-version:6.0
// Shared code for the Apple shells: the clip model the UI shows, the Keychain
// secret store, and the pasteboard adapters that sit between NSPasteboard and
// ctrl-core.
//
// Depends on the CtrlCore package that scripts/build-xcframework.sh writes to
// bindings/apple. Run that script before opening the project.
import PackageDescription

let package = Package(
    name: "CtrlKit",
    platforms: [.macOS(.v15), .iOS(.v18)],
    products: [.library(name: "CtrlKit", targets: ["CtrlKit"])],
    dependencies: [.package(path: "../../bindings/apple")],
    targets: [
        .target(name: "CtrlKit", dependencies: [.product(name: "CtrlCore", package: "apple")]),
        .testTarget(name: "CtrlKitTests", dependencies: ["CtrlKit"]),
    ]
)
