import Foundation
import FoundationModels

/// Serializes all Foundation Models calls through a single actor to prevent
/// concurrent access to the ObjC internals, which can cause SIGTRAP/SIGSEGV.
/// Same pattern that fixed NLEmbedding crashes.
private actor InferenceActor {
    static let shared = InferenceActor()

    private static let jsonOnlyInstruction =
        "\n\nYou must respond with valid JSON only. Do not include markdown code fences, explanations, or any text outside the JSON object."

    func respond(to request: ChatCompletionRequest) async throws -> String {
        let (instructions, userTurn) = buildPrompt(from: request.messages)
        let wantsJSON = request.responseFormat?.type == "json_object"

        var sessionInstructions = instructions
        if wantsJSON {
            sessionInstructions += Self.jsonOnlyInstruction
        }

        let session = LanguageModelSession(
            model: SystemLanguageModel.default,
            instructions: Instructions(sessionInstructions)
        )

        let response = try await session.respond(to: userTurn)
        let content = response.content

        if wantsJSON {
            return extractJSON(from: content)
        }
        return content
    }

    func stream(request: ChatCompletionRequest) -> AsyncThrowingStream<String, any Error> {
        AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    let (instructions, userTurn) = self.buildPrompt(from: request.messages)
                    let wantsJSON = request.responseFormat?.type == "json_object"

                    var sessionInstructions = instructions
                    if wantsJSON {
                        sessionInstructions += Self.jsonOnlyInstruction
                    }

                    let session = LanguageModelSession(
                        model: SystemLanguageModel.default,
                        instructions: Instructions(sessionInstructions)
                    )

                    var accumulated = ""
                    for try await partial in session.streamResponse(to: userTurn) {
                        let newText = String(partial.content.dropFirst(accumulated.count))
                        accumulated = partial.content
                        if !newText.isEmpty {
                            continuation.yield(newText)
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    private func buildPrompt(from messages: [ChatMessage]) -> (systemInstructions: String, userTurn: String) {
        var systemParts: [String] = []
        var conversationParts: [String] = []

        for message in messages {
            switch message.role {
            case "system":
                systemParts.append(message.content)
            case "assistant":
                conversationParts.append("Assistant: \(message.content)")
            default:
                conversationParts.append(message.content)
            }
        }

        let systemInstructions = systemParts.joined(separator: "\n\n")
        let userTurn = conversationParts.joined(separator: "\n\n")
        return (systemInstructions, userTurn.isEmpty ? "(empty)" : userTurn)
    }
}

/// Wraps the Apple Foundation Models framework, delegating all calls through
/// a serializing actor to prevent concurrent access crashes.
///
/// Checks availability at init time. If Foundation Models is not available (Apple
/// Intelligence disabled), `respond` and `stream` throw `LocalInferenceError.foundationModelsUnavailable`
/// instead of crashing or silently returning empty results. The `/v1/embeddings`
/// endpoint is unaffected by this guard.
struct FoundationModelsAdapter: InferenceAdapter {

    let isAvailable: Bool

    init() {
        self.isAvailable = SystemLanguageModel.default.isAvailable
    }

    func checkAvailability() throws {
        guard isAvailable else {
            throw LocalInferenceError.foundationModelsUnavailable
        }
    }

    func respond(to request: ChatCompletionRequest) async throws -> String {
        guard isAvailable else {
            throw LocalInferenceError.foundationModelsUnavailable
        }
        return try await InferenceActor.shared.respond(to: request)
    }

    func stream(request: ChatCompletionRequest) -> AsyncThrowingStream<String, any Error> {
        guard isAvailable else {
            return AsyncThrowingStream { $0.finish(throwing: LocalInferenceError.foundationModelsUnavailable) }
        }
        // Note: streaming must start on the actor but yields chunks back to the caller.
        // We wrap in a new stream that awaits the actor's stream.
        return AsyncThrowingStream { continuation in
            let task = Task {
                let innerStream = await InferenceActor.shared.stream(request: request)
                do {
                    for try await chunk in innerStream {
                        continuation.yield(chunk)
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}
