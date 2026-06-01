import Foundation
import HTTPTypes
import Hummingbird
import HummingbirdTesting
import NIOCore
import Testing

@testable import LocalInference

// MARK: - Mock adapter

/// Deterministic adapter for tests — returns whatever `response` is set to.
final class MockInferenceAdapter: InferenceAdapter, @unchecked Sendable {
    var response: String

    init(response: String = #"{"result":"ok"}"#) {
        self.response = response
    }

    func checkAvailability() throws {}

    func respond(to request: ChatCompletionRequest) async throws -> String {
        response
    }

    func stream(request: ChatCompletionRequest) -> AsyncThrowingStream<String, any Error> {
        let words = response.split(separator: " ").map(String.init)
        return AsyncThrowingStream { continuation in
            for word in words { continuation.yield(word + " ") }
            continuation.finish()
        }
    }
}

// MARK: - Helpers

private func makeApp(_ adapter: some InferenceAdapter) -> Application<RouterResponder<BasicRequestContext>> {
    Application(responder: buildRouter(adapter: adapter).buildResponder())
}

private func makeApp(_ adapter: some InferenceAdapter, embeddingActor: CoreMLEmbeddingActor?) -> Application<RouterResponder<BasicRequestContext>> {
    Application(responder: buildRouter(adapter: adapter, embeddingActor: embeddingActor).buildResponder())
}

private func postCompletions<T: Sendable>(
    _ client: some TestClientProtocol,
    body: String,
    _ closure: @escaping @Sendable (TestResponse) async throws -> T
) async throws -> T {
    try await client.execute(
        uri: "/v1/chat/completions",
        method: .post,
        headers: [.contentType: "application/json"],
        body: ByteBuffer(string: body),
        testCallback: closure
    )
}

// MARK: - Health

@Suite("Health endpoint")
struct HealthTests {
    @Test func healthReturns200() async throws {
        let app = makeApp(MockInferenceAdapter())
        try await app.test(.router) { client in
            try await client.execute(uri: "/health", method: .get) { response in
                #expect(response.status == .ok)
                let json = try JSONDecoder().decode([String: String].self, from: Data(response.body.readableBytesView))
                #expect(json["status"] == "ok")
                #expect(json["model"] == "apple-foundation")
            }
        }
    }
}

// MARK: - Blocking completions

@Suite("POST /v1/chat/completions — blocking")
struct BlockingCompletionTests {

    @Test func returnsOpenAIShape() async throws {
        let app = makeApp(MockInferenceAdapter(response: "Hello world"))
        try await app.test(.router) { client in
            try await postCompletions(client, body: """
                {"model":"apple-foundation","messages":[{"role":"user","content":"hi"}]}
                """) { response in
                #expect(response.status == .ok)
                #expect(response.headers[.contentType]?.contains("application/json") == true)

                let cr = try JSONDecoder().decode(ChatCompletionResponse.self,
                                                  from: Data(response.body.readableBytesView))
                #expect(cr.object == "chat.completion")
                #expect(cr.choices.count == 1)
                #expect(cr.choices[0].message.role == "assistant")
                #expect(cr.choices[0].message.content == "Hello world")
                #expect(cr.choices[0].finishReason == "stop")
                #expect(cr.usage.totalTokens == cr.usage.promptTokens + cr.usage.completionTokens)
            }
        }
    }

    @Test func modelFieldEchoed() async throws {
        let app = makeApp(MockInferenceAdapter())
        try await app.test(.router) { client in
            try await postCompletions(client, body: """
                {"model":"my-custom-name","messages":[{"role":"user","content":"hi"}]}
                """) { response in
                let cr = try JSONDecoder().decode(ChatCompletionResponse.self,
                                                  from: Data(response.body.readableBytesView))
                #expect(cr.model == "my-custom-name")
            }
        }
    }

    @Test func systemMessageAccepted() async throws {
        let app = makeApp(MockInferenceAdapter(response: "pong"))
        try await app.test(.router) { client in
            try await postCompletions(client, body: """
                {"model":"m","messages":[
                    {"role":"system","content":"You are helpful."},
                    {"role":"user","content":"ping"}
                ]}
                """) { response in
                #expect(response.status == .ok)
                let cr = try JSONDecoder().decode(ChatCompletionResponse.self,
                                                  from: Data(response.body.readableBytesView))
                #expect(cr.choices[0].message.content == "pong")
            }
        }
    }
}

// MARK: - JSON-mode stripping (extractJSON unit tests)

@Suite("extractJSON — fence and brace stripping")
struct ExtractJSONTests {

    @Test func plainJSONPassthrough() {
        let input = #"{"key":"value"}"#
        #expect(extractJSON(from: input) == input)
    }

    @Test func stripsJsonCodeFence() {
        let input = "```json\n{\"key\":\"value\"}\n```"
        #expect(extractJSON(from: input) == #"{"key":"value"}"#)
    }

    @Test func stripsPlainCodeFence() {
        let input = "```\n{\"key\":\"value\"}\n```"
        #expect(extractJSON(from: input) == #"{"key":"value"}"#)
    }

    @Test func stripsLeadingProse() {
        let input = "Here is the result: {\"key\":\"value\"} as requested."
        #expect(extractJSON(from: input) == #"{"key":"value"}"#)
    }

    @Test func noJSONPassthroughUnchanged() {
        // No braces — returns the cleaned string as-is
        let input = "just plain text"
        #expect(extractJSON(from: input) == "just plain text")
    }

    @Test func nestedObjectPreserved() {
        let input = #"{"outer":{"inner":1}}"#
        #expect(extractJSON(from: input) == input)
    }
}

// MARK: - JSON-mode via HTTP handler

@Suite("POST /v1/chat/completions — json_object response_format")
struct JSONModeTests {

    @Test func fenceStrippedInResponse() async throws {
        // Mock returns a response with a code fence; handler should strip it
        let raw = "```json\n{\"entities\":[]}\n```"
        let app = makeApp(MockInferenceAdapter(response: raw))
        try await app.test(.router) { client in
            try await postCompletions(client, body: """
                {
                    "model":"m",
                    "messages":[{"role":"user","content":"extract"}],
                    "response_format":{"type":"json_object"}
                }
                """) { response in
                #expect(response.status == .ok)
                let cr = try JSONDecoder().decode(ChatCompletionResponse.self,
                                                  from: Data(response.body.readableBytesView))
                let content = cr.choices[0].message.content
                // Content must be valid JSON and must not contain fences
                #expect(!content.contains("```"))
                let parsed = try JSONSerialization.jsonObject(with: content.data(using: .utf8)!)
                #expect(parsed is [String: Any])
            }
        }
    }

    @Test func cleanJSONPassesThroughUnchanged() async throws {
        let raw = #"{"entities":[{"name":"Marie Curie","type":"person"}]}"#
        let app = makeApp(MockInferenceAdapter(response: raw))
        try await app.test(.router) { client in
            try await postCompletions(client, body: """
                {
                    "model":"m",
                    "messages":[{"role":"user","content":"extract"}],
                    "response_format":{"type":"json_object"}
                }
                """) { response in
                let cr = try JSONDecoder().decode(ChatCompletionResponse.self,
                                                  from: Data(response.body.readableBytesView))
                #expect(cr.choices[0].message.content == raw)
            }
        }
    }
}

// MARK: - Embedding endpoint — no model loaded

@Suite("POST /v1/embeddings — no model")
struct EmbeddingNoModelTests {

    @Test func returns503WhenNoActor() async throws {
        let app = makeApp(MockInferenceAdapter(), embeddingActor: nil)
        try await app.test(.router) { client in
            try await client.execute(
                uri: "/v1/embeddings",
                method: .post,
                headers: [.contentType: "application/json"],
                body: ByteBuffer(string: #"{"input":"hello","model":"coreml-bge"}"#)
            ) { response in
                #expect(response.status == .serviceUnavailable)
            }
        }
    }

    @Test func returns503WhenNoActorRegardlessOfInput() async throws {
        // Actor-nil check fires before input validation — 503, not 400, even with empty input.
        let app = makeApp(MockInferenceAdapter(), embeddingActor: nil)
        try await app.test(.router) { client in
            try await client.execute(
                uri: "/v1/embeddings",
                method: .post,
                headers: [.contentType: "application/json"],
                body: ByteBuffer(string: #"{"input":[],"model":"coreml-bge"}"#)
            ) { response in
                #expect(response.status == .serviceUnavailable)
            }
        }
    }
}

// MARK: - Foundation Models unavailable → 503

@Suite("Foundation Models unavailable — 503")
struct FoundationModelsUnavailableTests {

    /// Adapter that simulates Apple Intelligence being disabled.
    private final class UnavailableAdapter: InferenceAdapter, @unchecked Sendable {
        func checkAvailability() throws {
            throw LocalInferenceError.foundationModelsUnavailable
        }
        func respond(to request: ChatCompletionRequest) async throws -> String {
            throw LocalInferenceError.foundationModelsUnavailable
        }
        func stream(request: ChatCompletionRequest) -> AsyncThrowingStream<String, any Error> {
            AsyncThrowingStream { $0.finish(throwing: LocalInferenceError.foundationModelsUnavailable) }
        }
    }

    @Test func chatCompletionsReturns503() async throws {
        let app = makeApp(UnavailableAdapter())
        try await app.test(.router) { client in
            try await client.execute(
                uri: "/v1/chat/completions",
                method: .post,
                headers: [.contentType: "application/json"],
                body: ByteBuffer(string: #"{"model":"apple-foundation","messages":[{"role":"user","content":"hi"}]}"#)
            ) { response in
                #expect(response.status == .serviceUnavailable)
                let err = try JSONDecoder().decode(ErrorResponse.self,
                                                   from: Data(response.body.readableBytesView))
                #expect(err.error.type == "server_error")
                #expect(!err.error.message.isEmpty)
            }
        }
    }

    @Test func streamingChatCompletionsReturns503() async throws {
        // checkAvailability() is called before committing SSE headers, so the
        // router can still map foundationModelsUnavailable → 503 for streaming requests.
        let app = makeApp(UnavailableAdapter())
        try await app.test(.router) { client in
            try await client.execute(
                uri: "/v1/chat/completions",
                method: .post,
                headers: [.contentType: "application/json"],
                body: ByteBuffer(string: #"{"model":"apple-foundation","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
            ) { response in
                #expect(response.status == .serviceUnavailable)
                let err = try JSONDecoder().decode(ErrorResponse.self,
                                                   from: Data(response.body.readableBytesView))
                #expect(err.error.type == "server_error")
                #expect(!err.error.message.isEmpty)
            }
        }
    }
}

// MARK: - Error handling

@Suite("Error handling")
struct ErrorHandlingTests {

    @Test func missingBodyReturns400() async throws {
        let app = makeApp(MockInferenceAdapter())
        try await app.test(.router) { client in
            // Send no body at all
            try await client.execute(
                uri: "/v1/chat/completions",
                method: .post,
                headers: [.contentType: "application/json"]
            ) { response in
                #expect(response.status == .badRequest)
            }
        }
    }

    @Test func malformedJSONReturns400() async throws {
        let app = makeApp(MockInferenceAdapter())
        try await app.test(.router) { client in
            try await postCompletions(client, body: "not json at all {{{{") { response in
                #expect(response.status == .badRequest)
            }
        }
    }

    @Test func unknownRouteReturns404() async throws {
        let app = makeApp(MockInferenceAdapter())
        try await app.test(.router) { client in
            try await client.execute(uri: "/v1/unknown", method: .get) { response in
                #expect(response.status == .notFound)
            }
        }
    }

    @Test func adapterErrorReturns500() async throws {
        final class ThrowingAdapter: InferenceAdapter, @unchecked Sendable {
            func checkAvailability() throws {}
            func respond(to request: ChatCompletionRequest) async throws -> String {
                throw NSError(domain: "test", code: 1, userInfo: [NSLocalizedDescriptionKey: "model unavailable"])
            }
            func stream(request: ChatCompletionRequest) -> AsyncThrowingStream<String, any Error> {
                AsyncThrowingStream { $0.finish(throwing: NSError(domain: "test", code: 1)) }
            }
        }

        let app = makeApp(ThrowingAdapter())
        try await app.test(.router) { client in
            try await postCompletions(client, body: """
                {"model":"m","messages":[{"role":"user","content":"hi"}]}
                """) { response in
                #expect(response.status == .internalServerError)
                let err = try JSONDecoder().decode(ErrorResponse.self,
                                                   from: Data(response.body.readableBytesView))
                #expect(err.error.type == "server_error")
                #expect(!err.error.message.isEmpty)
            }
        }
    }
}
