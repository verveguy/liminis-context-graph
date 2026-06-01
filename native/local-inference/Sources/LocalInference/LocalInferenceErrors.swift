import Foundation

/// Errors that can be thrown by the local-inference sidecar.
enum LocalInferenceError: Error, Sendable, Equatable {
    /// Foundation Models is not available on this device (Apple Intelligence disabled).
    /// The LLM endpoint returns 503 with this error; the embedding endpoint is unaffected.
    case foundationModelsUnavailable

    /// The embedding actor is not initialized (e.g. tests without a `.mlpackage`).
    /// Reserved for the genuine not-loaded state; never thrown for schema mismatches.
    case embeddingModelNotLoaded

    /// The loaded model's `last_hidden_state` dtype is not in the supported set.
    /// Thrown at startup from `CoreMLEmbeddingActor.init`, never per-request.
    case embeddingOutputDtypeUnsupported(observed: String, supported: [String])

    /// The loaded model's `last_hidden_state` shape does not match `[1, 512, 768]`.
    /// Thrown at startup from `CoreMLEmbeddingActor.init`, never per-request.
    case embeddingOutputShapeMismatch(observed: [Int], expected: [Int])

    /// The loaded model does not expose an output feature named `last_hidden_state`.
    /// Thrown at startup from `CoreMLEmbeddingActor.init`, never per-request.
    case embeddingOutputMissing(name: String)
}
