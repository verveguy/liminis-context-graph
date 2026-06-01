import CoreML
import CryptoKit
import Foundation

/// First-launch CoreML model cache utilities.
///
/// Owns the per-device cache directory at `<userData>/local-inference/`. Compiles
/// the bundled `.mlpackage` once and writes a `compiled.ok` sentinel after both the
/// directory rename and a sanity-check load succeed, so a crash mid-compile cannot
/// leave a half-written `.mlmodelc` that future launches mistake for a complete one.
///
/// Layout under cacheDir:
///   bge-base-en-v1.5.mlmodelc/   ← finalized compiled model
///   bge-base-en-v1.5.mlmodelc.tmp/  ← only present during compile, never read
///   compiled.ok                  ← sentinel; contains build hash of source .mlpackage
enum SetupCache {
    static let compiledModelName = "bge-base-en-v1.5.mlmodelc"
    static let tempModelName = "bge-base-en-v1.5.mlmodelc.tmp"
    static let sentinelName = "compiled.ok"

    enum SetupCacheError: Error {
        case sentinelWriteRace
        case sentinelWriteFailed(String)
    }

    /// Compute a build hash for the source `.mlpackage` that is stable across launches
    /// but changes when the model weights change. Hashes the model weights file only
    /// (`Data/com.apple.CoreML/weights/weight.bin`) — tens-of-ms per startup is fine,
    /// hashing every file in the directory would be wasteful.
    ///
    /// Returns nil if the weights file is unreadable. In that case `cacheIsValid`
    /// accepts any existing `compiled.ok` sentinel (the cache is trusted without
    /// hash verification), so model upgrades cannot auto-invalidate the cache
    /// while the source weights remain unreadable. This is a rare edge case;
    /// recovery is to delete the cache directory manually.
    static func buildHash(for mlpackageURL: URL) -> String? {
        let weightsURL = mlpackageURL
            .appendingPathComponent("Data")
            .appendingPathComponent("com.apple.CoreML")
            .appendingPathComponent("weights")
            .appendingPathComponent("weight.bin")

        guard let handle = try? FileHandle(forReadingFrom: weightsURL) else {
            return nil
        }
        defer { try? handle.close() }

        var hasher = SHA256()
        let chunkSize = 1 << 20 // 1 MiB
        while true {
            let chunk = handle.readData(ofLength: chunkSize)
            if chunk.isEmpty { break }
            hasher.update(data: chunk)
        }
        let digest = hasher.finalize()
        return digest.map { String(format: "%02x", $0) }.joined()
    }

    /// Compiled model URL inside the cache (always returned, may not exist).
    static func compiledModelURL(in cacheDir: URL) -> URL {
        cacheDir.appendingPathComponent(compiledModelName)
    }

    /// Sentinel file URL inside the cache (always returned, may not exist).
    static func sentinelURL(in cacheDir: URL) -> URL {
        cacheDir.appendingPathComponent(sentinelName)
    }

    /// Check whether the cache contains a valid compiled model matching the source's build hash.
    ///
    /// Returns true only if:
    ///   1. `<cacheDir>/bge-base-en-v1.5.mlmodelc` exists as a directory
    ///   2. `<cacheDir>/compiled.ok` exists
    ///   3. Sentinel content matches the supplied expected hash (when both are non-nil)
    ///
    /// When `expectedHash` is nil (couldn't read source weights), accept any sentinel.
    static func cacheIsValid(cacheDir: URL, expectedHash: String?) -> Bool {
        let fm = FileManager.default
        let modelURL = compiledModelURL(in: cacheDir)
        let sentURL = sentinelURL(in: cacheDir)

        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: modelURL.path, isDirectory: &isDir), isDir.boolValue else {
            return false
        }
        guard fm.fileExists(atPath: sentURL.path) else {
            return false
        }
        guard let expected = expectedHash else {
            return true
        }
        guard let cached = try? String(contentsOf: sentURL, encoding: .utf8) else {
            return false
        }
        return cached.trimmingCharacters(in: .whitespacesAndNewlines) == expected
    }

    /// Recursively delete any stale model directories or sentinel from a previous
    /// crashed compile. Safe to call when the cache is empty or partially populated.
    static func cleanStaleCache(cacheDir: URL) {
        let fm = FileManager.default
        let candidates = [
            compiledModelURL(in: cacheDir),
            cacheDir.appendingPathComponent(tempModelName),
            sentinelURL(in: cacheDir),
        ]
        for url in candidates {
            try? fm.removeItem(at: url)
        }
    }

    /// Ensure the cache directory exists (creates intermediates as needed).
    static func ensureCacheDir(_ cacheDir: URL) throws {
        try FileManager.default.createDirectory(at: cacheDir, withIntermediateDirectories: true)
    }

    /// Move a freshly-compiled `.mlmodelc` (typically in a temp directory created by
    /// `MLModel.compileModel(at:)`) into the cache via an atomic rename through a
    /// `.tmp` staging path inside the cache directory.
    ///
    /// Steps:
    ///   1. Remove any pre-existing `bge-base-en-v1.5.mlmodelc.tmp` (left by a previous crash).
    ///   2. Copy the compiled model from its source location to `<cacheDir>/...mlmodelc.tmp`.
    ///   3. Atomically rename `...mlmodelc.tmp` → `...mlmodelc`.
    ///
    /// We copy rather than move from the source because `MLModel.compileModel(at:)`
    /// returns a path in the system temp directory that may be on a different volume
    /// from `<userData>`, in which case `moveItem` falls back to copy+delete anyway.
    static func installCompiledModel(from compiledSourceURL: URL, into cacheDir: URL) throws -> URL {
        let fm = FileManager.default
        let tmpURL = cacheDir.appendingPathComponent(tempModelName)
        let finalURL = compiledModelURL(in: cacheDir)

        try? fm.removeItem(at: tmpURL)
        try? fm.removeItem(at: finalURL)

        try fm.copyItem(at: compiledSourceURL, to: tmpURL)
        try fm.moveItem(at: tmpURL, to: finalURL)
        return finalURL
    }

    /// Known setup stages emitted on stderr as `[setup] <stage>[ <message>]`. The
    /// Electron lifecycle parser whitelists these so unknown stages aren't
    /// forwarded to the renderer.
    static let knownStages: Set<String> = [
        "copying_model",
        "compiling_model",
        "installing_model",
        "verifying_model",
        "model_ready",
        "setup_failed",
    ]

    /// Write the sentinel file atomically using `O_CREAT | O_EXCL | O_WRONLY` so a
    /// racing writer cannot observe a partial file. The sentinel body is the build hash
    /// (or "unknown" if the source weights were unreadable). Throws if a sentinel
    /// already exists at the path.
    static func writeSentinel(at sentURL: URL, buildHash: String?) throws {
        let body = buildHash ?? "unknown"
        let path = sentURL.path
        let fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0o644)
        if fd < 0 {
            if errno == EEXIST {
                throw SetupCacheError.sentinelWriteRace
            }
            throw SetupCacheError.sentinelWriteFailed("open failed (errno=\(errno))")
        }
        defer { close(fd) }
        let bytes = Array(body.utf8)
        let written = bytes.withUnsafeBufferPointer { buf -> Int in
            write(fd, buf.baseAddress, buf.count)
        }
        if written != bytes.count {
            throw SetupCacheError.sentinelWriteFailed("short write (\(written)/\(bytes.count))")
        }
    }
}

/// Emit a one-line setup-progress event on stderr in the form
/// `[setup] <stage>[ <message>]\n` so the Electron lifecycle parser can forward
/// it to the renderer via the `workspace:localInferenceProgress` IPC channel.
extension FileHandle {
    func emitSetupEvent(stage: String, message: String? = nil) {
        let line: String
        if let msg = message {
            line = "[setup] \(stage) \(msg)\n"
        } else {
            line = "[setup] \(stage)\n"
        }
        if let data = line.data(using: .utf8) {
            self.write(data)
        }
    }
}
