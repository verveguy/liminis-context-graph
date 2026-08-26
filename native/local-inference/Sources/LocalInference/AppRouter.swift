import Foundation
import Hummingbird

/// Build the application router, injecting the inference backend.
/// Used by both `main.swift` (with `FoundationModelsAdapter`) and the
/// test suite (with `MockInferenceAdapter`).
///
/// - Parameters:
///   - adapter: The LLM inference backend (Foundation Models in production, mock in tests).
///   - embeddingActor: The CoreML embedding actor. When nil (tests without the .mlpackage),
///     the `/v1/embeddings` route returns 503.
///   - mode: Which capability set to register routes for. Defaults to `.both`, matching
///     pre-existing behavior. A capability not included in `mode` gets a 404 stub instead
///     of its normal handler, distinguishing "disabled by configuration" from "enabled but
///     currently unavailable" (e.g. the 503s above).
func buildRouter(
    adapter: some InferenceAdapter,
    embeddingActor: CoreMLEmbeddingActor? = nil,
    mode: LocalInferenceMode = .both
) -> Router<BasicRequestContext> {
    let router = Router()

    router.get("/health") { request, context in
        try await handleHealth(request: request, context: context)
    }

    guard mode.includesCompletions else {
        router.post("/v1/chat/completions") { _, _ in
            makeErrorResponse(
                status: .notFound,
                message: "/v1/chat/completions is disabled: LOCAL_INFERENCE_MODE=\(mode.rawValue) does not include completions."
            )
        }
        return registerEmbeddingsRoute(router, embeddingActor: embeddingActor, mode: mode)
    }

    router.post("/v1/chat/completions") { request, context in
        do {
            return try await handleChatCompletions(request: request, context: context, adapter: adapter)
        } catch let err as LocalInferenceError where err == .foundationModelsUnavailable {
            return makeErrorResponse(
                status: .serviceUnavailable,
                message: "Apple Foundation Models is not available on this device. Enable Apple Intelligence in System Settings."
            )
        } catch {
            fputs("[local-inference] Unhandled chat completion error: \(error)\n", stderr)
            return makeErrorResponse(status: .internalServerError, message: "An internal error occurred")
        }
    }

    return registerEmbeddingsRoute(router, embeddingActor: embeddingActor, mode: mode)
}

/// Registers `/v1/embeddings`, either the real handler (mode includes embeddings) or a
/// 404 stub (mode does not) — factored out so both `buildRouter` early-return paths share it.
private func registerEmbeddingsRoute(
    _ router: Router<BasicRequestContext>,
    embeddingActor: CoreMLEmbeddingActor?,
    mode: LocalInferenceMode
) -> Router<BasicRequestContext> {
    guard mode.includesEmbeddings else {
        router.post("/v1/embeddings") { _, _ in
            makeErrorResponse(
                status: .notFound,
                message: "/v1/embeddings is disabled: LOCAL_INFERENCE_MODE=\(mode.rawValue) does not include embeddings."
            )
        }
        return router
    }

    router.post("/v1/embeddings") { request, context in
        guard let actor = embeddingActor else {
            return makeErrorResponse(
                status: .serviceUnavailable,
                message: "CoreML embedding model not loaded. Start the sidecar with LOCAL_INFERENCE_EMBEDDING_MODEL set."
            )
        }
        do {
            return try await handleEmbeddings(request: request, context: context, actor: actor)
        } catch let err as LocalInferenceError {
            // Schema-mismatch errors are normally caught at startup by
            // CoreMLEmbeddingActor.validateOutputSchema and never reach here. The arms
            // exist defensively so that if the model on disk changes between startup
            // and a request, the caller still gets a specific message instead of the
            // generic "internal error".
            switch err {
            case .embeddingOutputDtypeUnsupported(let observed, let supported):
                fputs("[local-inference] Embedding output dtype unsupported: observed=\(observed) supported=\(supported)\n", stderr)
                return makeErrorResponse(
                    status: .internalServerError,
                    message: "Embedding model output dtype '\(observed)' is not supported. Supported dtypes: \(supported.joined(separator: ", "))."
                )
            case .embeddingOutputShapeMismatch(let observed, let expected):
                fputs("[local-inference] Embedding output shape mismatch: observed=\(observed) expected=\(expected)\n", stderr)
                return makeErrorResponse(
                    status: .internalServerError,
                    message: "Embedding model output shape mismatch. Observed \(observed), expected \(expected)."
                )
            case .embeddingOutputMissing(let name):
                fputs("[local-inference] Embedding output missing: name=\(name)\n", stderr)
                return makeErrorResponse(
                    status: .internalServerError,
                    message: "Embedding model is missing required output '\(name)'."
                )
            case .embeddingModelNotLoaded, .foundationModelsUnavailable:
                fputs("[local-inference] Unhandled embedding error: \(err)\n", stderr)
                return makeErrorResponse(status: .internalServerError, message: "An internal error occurred")
            }
        } catch {
            fputs("[local-inference] Unhandled embedding error: \(error)\n", stderr)
            return makeErrorResponse(status: .internalServerError, message: "An internal error occurred")
        }
    }

    return router
}
