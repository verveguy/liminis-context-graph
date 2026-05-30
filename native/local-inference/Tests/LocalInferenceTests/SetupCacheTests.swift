#if os(macOS)

import Foundation
import Testing

@testable import LocalInference

@Suite("SetupCache")
struct SetupCacheTests {
    // MARK: - Helpers

    /// Create a unique scratch directory under NSTemporaryDirectory(), returning the URL.
    /// Caller is responsible for cleanup (typically via the deinit on a `ScratchDir` wrapper,
    /// but Swift Testing structs don't have one — tests `defer` removal manually).
    private static func makeScratchDir() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("setup-cache-tests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    /// Build a fake compiled `.mlmodelc` directory (just contents on disk — not
    /// actually loadable by CoreML, but valid for cache plumbing tests).
    private static func makeFakeCompiledSource(in parent: URL) throws -> URL {
        let url = parent.appendingPathComponent("fake-compiled-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        try Data("model-bytes".utf8).write(to: url.appendingPathComponent("model.bin"))
        return url
    }

    // MARK: - cacheIsValid

    @Test func cacheIsValidFalseWhenEmpty() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        #expect(SetupCache.cacheIsValid(cacheDir: cacheDir, expectedHash: "abc") == false)
    }

    @Test func cacheIsValidFalseWhenModelExistsButNoSentinel() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        try FileManager.default.createDirectory(
            at: SetupCache.compiledModelURL(in: cacheDir),
            withIntermediateDirectories: true
        )
        #expect(SetupCache.cacheIsValid(cacheDir: cacheDir, expectedHash: "abc") == false)
    }

    @Test func cacheIsValidFalseWhenSentinelExistsButNoModel() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        try Data("abc".utf8).write(to: SetupCache.sentinelURL(in: cacheDir))
        #expect(SetupCache.cacheIsValid(cacheDir: cacheDir, expectedHash: "abc") == false)
    }

    @Test func cacheIsValidFalseWhenSentinelHashMismatches() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        try FileManager.default.createDirectory(
            at: SetupCache.compiledModelURL(in: cacheDir),
            withIntermediateDirectories: true
        )
        try Data("old-hash".utf8).write(to: SetupCache.sentinelURL(in: cacheDir))
        #expect(SetupCache.cacheIsValid(cacheDir: cacheDir, expectedHash: "new-hash") == false)
    }

    @Test func cacheIsValidTrueWhenHashMatches() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        try FileManager.default.createDirectory(
            at: SetupCache.compiledModelURL(in: cacheDir),
            withIntermediateDirectories: true
        )
        try Data("matching-hash".utf8).write(to: SetupCache.sentinelURL(in: cacheDir))
        #expect(SetupCache.cacheIsValid(cacheDir: cacheDir, expectedHash: "matching-hash") == true)
    }

    @Test func cacheIsValidTrueWhenExpectedHashNilAndSentinelPresent() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        try FileManager.default.createDirectory(
            at: SetupCache.compiledModelURL(in: cacheDir),
            withIntermediateDirectories: true
        )
        try Data("any".utf8).write(to: SetupCache.sentinelURL(in: cacheDir))
        #expect(SetupCache.cacheIsValid(cacheDir: cacheDir, expectedHash: nil) == true)
    }

    // MARK: - cleanStaleCache

    @Test func cleanStaleCacheRemovesPartialDirsAndSentinel() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }

        let modelURL = SetupCache.compiledModelURL(in: cacheDir)
        let tmpURL = cacheDir.appendingPathComponent(SetupCache.tempModelName)
        let sentURL = SetupCache.sentinelURL(in: cacheDir)
        try FileManager.default.createDirectory(at: modelURL, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: tmpURL, withIntermediateDirectories: true)
        try Data("x".utf8).write(to: sentURL)

        SetupCache.cleanStaleCache(cacheDir: cacheDir)

        #expect(FileManager.default.fileExists(atPath: modelURL.path) == false)
        #expect(FileManager.default.fileExists(atPath: tmpURL.path) == false)
        #expect(FileManager.default.fileExists(atPath: sentURL.path) == false)
    }

    @Test func cleanStaleCacheIsNoOpWhenAlreadyEmpty() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        SetupCache.cleanStaleCache(cacheDir: cacheDir)
        #expect(FileManager.default.fileExists(atPath: cacheDir.path) == true)
    }

    // MARK: - installCompiledModel

    @Test func installCompiledModelCopiesIntoCacheAtFinalPath() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        let source = try Self.makeFakeCompiledSource(in: cacheDir.deletingLastPathComponent())
        defer { try? FileManager.default.removeItem(at: source) }

        let finalURL = try SetupCache.installCompiledModel(from: source, into: cacheDir)

        #expect(finalURL.path == SetupCache.compiledModelURL(in: cacheDir).path)
        #expect(FileManager.default.fileExists(atPath: finalURL.path) == true)
        let modelBin = finalURL.appendingPathComponent("model.bin")
        let bytes = try Data(contentsOf: modelBin)
        #expect(String(data: bytes, encoding: .utf8) == "model-bytes")
        // Temp directory inside the cache should be gone after the rename
        let tmpURL = cacheDir.appendingPathComponent(SetupCache.tempModelName)
        #expect(FileManager.default.fileExists(atPath: tmpURL.path) == false)
    }

    @Test func installCompiledModelReplacesExistingFinalDir() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        // Pre-populate the final path with junk so we exercise the cleanup branch
        let finalPath = SetupCache.compiledModelURL(in: cacheDir)
        try FileManager.default.createDirectory(at: finalPath, withIntermediateDirectories: true)
        try Data("stale".utf8).write(to: finalPath.appendingPathComponent("stale.bin"))

        let source = try Self.makeFakeCompiledSource(in: cacheDir.deletingLastPathComponent())
        defer { try? FileManager.default.removeItem(at: source) }

        _ = try SetupCache.installCompiledModel(from: source, into: cacheDir)

        // The stale file should be gone; the fresh model.bin should be present
        #expect(FileManager.default.fileExists(atPath: finalPath.appendingPathComponent("stale.bin").path) == false)
        #expect(FileManager.default.fileExists(atPath: finalPath.appendingPathComponent("model.bin").path) == true)
    }

    // MARK: - writeSentinel

    @Test func writeSentinelCreatesFileWithHashContent() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        let sentURL = SetupCache.sentinelURL(in: cacheDir)
        try SetupCache.writeSentinel(at: sentURL, buildHash: "deadbeef")
        let body = try String(contentsOf: sentURL, encoding: .utf8)
        #expect(body == "deadbeef")
    }

    @Test func writeSentinelWritesUnknownWhenHashNil() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        let sentURL = SetupCache.sentinelURL(in: cacheDir)
        try SetupCache.writeSentinel(at: sentURL, buildHash: nil)
        let body = try String(contentsOf: sentURL, encoding: .utf8)
        #expect(body == "unknown")
    }

    @Test func writeSentinelFailsWhenAlreadyExists() throws {
        let cacheDir = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: cacheDir) }
        let sentURL = SetupCache.sentinelURL(in: cacheDir)
        try Data("first".utf8).write(to: sentURL)

        do {
            try SetupCache.writeSentinel(at: sentURL, buildHash: "second")
            Issue.record("Expected writeSentinel to throw when sentinel already exists")
        } catch SetupCache.SetupCacheError.sentinelWriteRace {
            // Expected
        }
    }

    // MARK: - buildHash

    @Test func buildHashReturnsNilWhenWeightsMissing() throws {
        let scratch = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: scratch) }
        // mlpackage dir with no Data/com.apple.CoreML/weights/weight.bin
        let mlpackage = scratch.appendingPathComponent("empty.mlpackage")
        try FileManager.default.createDirectory(at: mlpackage, withIntermediateDirectories: true)
        #expect(SetupCache.buildHash(for: mlpackage) == nil)
    }

    @Test func buildHashIsStableAndChangesWithContent() throws {
        let scratch = try Self.makeScratchDir()
        defer { try? FileManager.default.removeItem(at: scratch) }

        func makePkg(name: String, contents: String) throws -> URL {
            let pkg = scratch.appendingPathComponent(name)
            let weightsDir = pkg
                .appendingPathComponent("Data")
                .appendingPathComponent("com.apple.CoreML")
                .appendingPathComponent("weights")
            try FileManager.default.createDirectory(at: weightsDir, withIntermediateDirectories: true)
            try Data(contents.utf8).write(to: weightsDir.appendingPathComponent("weight.bin"))
            return pkg
        }

        let a1 = try makePkg(name: "a.mlpackage", contents: "AAAA")
        let a2 = try makePkg(name: "a-copy.mlpackage", contents: "AAAA")
        let b = try makePkg(name: "b.mlpackage", contents: "BBBB")

        let hashA1 = SetupCache.buildHash(for: a1)
        let hashA2 = SetupCache.buildHash(for: a2)
        let hashB = SetupCache.buildHash(for: b)

        #expect(hashA1 != nil)
        #expect(hashA1 == hashA2)
        #expect(hashA1 != hashB)
    }
}

#endif
