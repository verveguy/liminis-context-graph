// CoreML and the stub model fixture are macOS-only; the entire suite compiles
// away on Linux so Swift cross-compilation and Linux CI remain unaffected.
#if os(macOS)

import Foundation
import HTTPTypes
import Hummingbird
import HummingbirdTesting
import NIOCore
import Testing

@testable import LocalInference

// MARK: - Tokenizer cache layout note
//
// swift-transformers HubApi stores files at:
//   <downloadBase>/models/<org>/<model>/      ← flat path, slashes preserved
//
// Python huggingface_hub uses a different layout:
//   <downloadBase>/models--<ORG>--<model>/snapshots/<hash>/
//
// The fixture at Fixtures/tokenizer-cache/ uses the swift-transformers format.
// Do NOT restructure it to match Python's double-dash convention.

// MARK: - Integration suite

// NOTE: 503 when embeddingActor is nil is covered by EmbeddingNoModelTests
// in LocalInferenceTests.swift — it is not duplicated here.

/// Each fixture exercises one of the two supported CoreML output dtypes
/// (fp32 + fp16). Parametrizing the suite (FR-008) means every assertion runs
/// against both, guarding the per-request dtype dispatch.
enum StubFixture: String, CaseIterable, Sendable, CustomStringConvertible {
    case fp32 = "stub-bge-base.mlpackage"
    case fp16 = "stub-bge-base-fp16.mlpackage"

    var description: String { rawValue }
}

@Suite("POST /v1/embeddings — integration")
struct EmbeddingIntegrationTests {
    /// Build the actor + app for a specific fixture. Called from each @Test so
    /// Swift Testing parametrization can drive each fixture through every case.
    /// MLModel.compileModel(at:) is fast for the 0.75–1.5 MB stubs — expect < 2 s.
    private static func makeApp(
        for fixture: StubFixture
    ) async throws -> (CoreMLEmbeddingActor, Application<RouterResponder<BasicRequestContext>>) {
        guard let resourceRoot = Bundle.module.resourceURL else {
            throw SetupError.missingResourceBundle
        }
        let fixturesURL = resourceRoot.appendingPathComponent("Fixtures")
        let modelURL = fixturesURL.appendingPathComponent(fixture.rawValue)
        let cachePath = fixturesURL.appendingPathComponent("tokenizer-cache").path

        let actor = try await CoreMLEmbeddingActor(
            modelURL: modelURL,
            tokenizerModelId: "BAAI/bge-base-en-v1.5",
            hubCachePath: cachePath
        )
        let app = Application(
            responder: buildRouter(adapter: StubAdapter(), embeddingActor: actor).buildResponder()
        )
        return (actor, app)
    }

    // MARK: - Tests

    @Test(arguments: StubFixture.allCases)
    func singleInputReturns200WithUnitNormEmbedding(fixture: StubFixture) async throws {
        let (_, app) = try await Self.makeApp(for: fixture)
        try await app.test(.router) { client in
            let response = try await postEmbeddings(client, body: #"{"input":"hello world","model":"coreml-bge"}"#)
            #expect(response.status == .ok)
            let resp = try decode(EmbeddingResponse.self, from: response)
            #expect(resp.data.count == 1)
            let embedding = resp.data[0].embedding
            #expect(embedding.count == 768)
            let norm = sqrt(embedding.reduce(0.0) { $0 + $1 * $1 })
            #expect(abs(norm - 1.0) < 1e-5, "Expected unit L2 norm for \(fixture), got \(norm)")
        }
    }

    @Test(arguments: StubFixture.allCases)
    func batchInputReturns200WithCorrectCount(fixture: StubFixture) async throws {
        let (_, app) = try await Self.makeApp(for: fixture)
        try await app.test(.router) { client in
            let response = try await postEmbeddings(client, body: #"{"input":["a","b","c"],"model":"coreml-bge"}"#)
            #expect(response.status == .ok)
            let resp = try decode(EmbeddingResponse.self, from: response)
            #expect(resp.data.count == 3)
            for item in resp.data {
                #expect(item.embedding.count == 768)
            }
        }
    }

    @Test(arguments: StubFixture.allCases)
    func emptyStringInBatchReturnsZeroVector(fixture: StubFixture) async throws {
        // REQ-12: empty string → 768-element zero vector, not an error.
        // embed() short-circuits before MLModel.prediction() for empty inputs.
        let (_, app) = try await Self.makeApp(for: fixture)
        try await app.test(.router) { client in
            let response = try await postEmbeddings(client, body: #"{"input":[""],"model":"coreml-bge"}"#)
            #expect(response.status == .ok)
            let resp = try decode(EmbeddingResponse.self, from: response)
            #expect(resp.data.count == 1)
            let embedding = resp.data[0].embedding
            #expect(embedding.count == 768)
            #expect(embedding.allSatisfy { $0 == 0.0 }, "Expected zero vector for empty-string input")
        }
    }

    @Test(arguments: StubFixture.allCases)
    func emptyArrayReturns400(fixture: StubFixture) async throws {
        // handleEmbeddings() guard fires before actor.embed() — returns 400, not 503.
        let (_, app) = try await Self.makeApp(for: fixture)
        try await app.test(.router) { client in
            let response = try await postEmbeddings(client, body: #"{"input":[],"model":"coreml-bge"}"#)
            #expect(response.status == .badRequest)
        }
    }

    @Test(arguments: StubFixture.allCases)
    func concurrentRequestsAllSucceed(fixture: StubFixture) async throws {
        // N=5 concurrent requests queue on the actor serially; all must complete
        // with HTTP 200 and a correctly shaped embedding.
        let n = 5
        let (_, app) = try await Self.makeApp(for: fixture)
        try await app.test(.router) { client in
            try await withThrowingTaskGroup(of: Void.self) { group in
                for _ in 0..<n {
                    group.addTask {
                        let response = try await postEmbeddings(
                            client,
                            body: #"{"input":"concurrent test","model":"coreml-bge"}"#
                        )
                        #expect(response.status == .ok)
                        let resp = try decode(EmbeddingResponse.self, from: response)
                        #expect(resp.data.count == 1)
                        #expect(resp.data[0].embedding.count == 768)
                    }
                }
                try await group.waitForAll()
            }
        }
    }

    // MARK: - Perf smoke test (FR-007)
    //
    // A single embedding request against the fp16 fixture must complete under a
    // generous wall-clock ceiling. This is a smoke test, not a strict 5%
    // comparison — that level of precision belongs in a benchmark, not the test
    // suite. The ceiling is intentionally wide to absorb CI noise on shared
    // runners; if the half→float conversion ever regresses by orders of
    // magnitude (e.g. someone replaces vImage with a Swift element-loop), this
    // will catch it.

    @Test func fp16PathLatencyUnderCeiling() async throws {
        let (_, app) = try await Self.makeApp(for: .fp16)
        try await app.test(.router) { client in
            // Warm-up: first prediction includes ANE compilation and cache warm-up.
            _ = try await postEmbeddings(client, body: #"{"input":"warmup","model":"coreml-bge"}"#)

            let start = ContinuousClock.now
            let response = try await postEmbeddings(client, body: #"{"input":"latency probe","model":"coreml-bge"}"#)
            let elapsed = ContinuousClock.now - start

            #expect(response.status == .ok)
            #expect(elapsed < .milliseconds(500), "fp16 single-request latency \(elapsed) exceeded ceiling")
        }
    }
}

// MARK: - Helpers

private enum SetupError: Error {
    case missingResourceBundle
}

/// Minimal inference adapter — integration tests only exercise /v1/embeddings.
private final class StubAdapter: InferenceAdapter, @unchecked Sendable {
    func checkAvailability() throws {}
    func respond(to request: ChatCompletionRequest) async throws -> String { "" }
    func stream(request: ChatCompletionRequest) -> AsyncThrowingStream<String, any Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

private func postEmbeddings(
    _ client: some TestClientProtocol,
    body: String
) async throws -> TestResponse {
    try await client.execute(
        uri: "/v1/embeddings",
        method: .post,
        headers: [.contentType: "application/json"],
        body: ByteBuffer(string: body)
    ) { $0 }
}

private func decode<T: Decodable>(_ type: T.Type, from response: TestResponse) throws -> T {
    try JSONDecoder().decode(type, from: Data(response.body.readableBytesView))
}

#endif
