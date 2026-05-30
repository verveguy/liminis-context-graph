import Foundation
import Hummingbird
import FoundationModels

// MARK: - Availability guard

guard #available(macOS 26, *) else {
    fputs("Error: local-inference requires macOS 26 or later\n", stderr)
    exit(1)
}

// MARK: - Configuration

let defaultSocketPath = "/tmp/liminis-inference.sock"
let socketPath = ProcessInfo.processInfo.environment["LOCAL_INFERENCE_SOCKET"]
    ?? defaultSocketPath

// MARK: - CoreML model path validation (REQ-09)
// The sidecar exits with a clear error if the embedding model is missing.
// Silent fallback to NLEmbedding is forbidden — it would silently regress quality.

let defaultModelPath = "./bge-base-en-v1.5.mlpackage"
let modelPath = URL(fileURLWithPath: ProcessInfo.processInfo.environment["LOCAL_INFERENCE_EMBEDDING_MODEL"]
    ?? defaultModelPath).standardized.path
let modelURL = URL(fileURLWithPath: modelPath)

// HuggingFace Hub local cache: structure must be <cachePath>/models/<org>/<model>/.
// If LOCAL_INFERENCE_HF_CACHE is set, tokenizer loads offline from that directory.
// If unset, swift-transformers downloads the tokenizer from HuggingFace Hub on first run.
// Canonicalize via URL.standardized to resolve any `..` components before passing to
// HubApi — workaround for swift-transformers v0.1.24 substring-match bug in snapshot().
let hubCachePath: String? = ProcessInfo.processInfo.environment["LOCAL_INFERENCE_HF_CACHE"]
    .map { URL(fileURLWithPath: $0).standardized.path }
let tokenizerModelId = ProcessInfo.processInfo.environment["LOCAL_INFERENCE_TOKENIZER_ID"]
    ?? "BAAI/bge-base-en-v1.5"

// First-launch cache for the compiled `.mlmodelc`. When set, the sidecar compiles
// the bundled `.mlpackage` once, persists the result under this directory, and
// loads from cache on subsequent launches. Owned by Electron; the sidecar creates
// the directory if missing and emits `[setup]` progress lines on stderr.
let mlmodelcCacheDir: URL? = ProcessInfo.processInfo.environment["LOCAL_INFERENCE_MLMODELC_CACHE"]
    .map { URL(fileURLWithPath: $0).standardized }

guard FileManager.default.fileExists(atPath: modelPath) else {
    fputs("Error: CoreML embedding model not found at \(modelPath)\n", stderr)
    fputs("Run: uv run convert-embedding-model.py to generate it, then set LOCAL_INFERENCE_EMBEDDING_MODEL.\n", stderr)
    exit(1)
}

print("local-inference: embedding model path: \(modelPath)")
print("local-inference: tokenizer: \(tokenizerModelId), HF cache: \(hubCachePath ?? "online (no LOCAL_INFERENCE_HF_CACHE set)")")

// MARK: - Foundation Models availability
// The guard moved out of startup — FoundationModelsAdapter checks at call time.
// The sidecar starts even if Apple Intelligence is disabled; only /v1/chat/completions
// is degraded (returns 503). The /v1/embeddings endpoint is unaffected.

let fmAdapter = FoundationModelsAdapter()
if fmAdapter.isAvailable {
    print("local-inference: Apple Foundation Models ready")
} else {
    print("local-inference: Apple Foundation Models NOT available (Apple Intelligence disabled)")
    print("local-inference: /v1/chat/completions will return 503; /v1/embeddings unaffected")
}

// MARK: - CoreML embedding actor initialization
// Wrap in do-catch so tokenizer/compile failures print a clear message instead
// of the default "Fatal error: Error raised at top level" Swift runtime message.

let embeddingActor: CoreMLEmbeddingActor
do {
    embeddingActor = try await CoreMLEmbeddingActor(
        modelURL: modelURL,
        tokenizerModelId: tokenizerModelId,
        hubCachePath: hubCachePath,
        mlmodelcCacheDir: mlmodelcCacheDir
    )
} catch {
    // Surface the failure on the [setup] channel so the onboarding wizard can
    // show the actual reason, then print the legacy diagnostic for the log file.
    FileHandle.standardError.emitSetupEvent(stage: "setup_failed", message: "\(error)")
    fputs("Error: Failed to initialize CoreML embedding actor: \(error)\n", stderr)
    if hubCachePath != nil {
        fputs("Check that LOCAL_INFERENCE_HF_CACHE points to a valid HuggingFace cache directory.\n", stderr)
    } else {
        fputs("Set LOCAL_INFERENCE_HF_CACHE to a local HF cache, or ensure network access for online download.\n", stderr)
    }
    fputs("Also check that LOCAL_INFERENCE_EMBEDDING_MODEL is a valid .mlpackage or .mlmodelc path.\n", stderr)
    exit(1)
}

// Remove stale socket file so Hummingbird can bind cleanly on restart
try? FileManager.default.removeItem(atPath: socketPath)

// MARK: - Application

let app = Application(
    router: buildRouter(adapter: fmAdapter, embeddingActor: embeddingActor),
    configuration: .init(address: .unixDomainSocket(path: socketPath))
)

print("local-inference: listening on unix:\(socketPath)")

try await app.runService()
