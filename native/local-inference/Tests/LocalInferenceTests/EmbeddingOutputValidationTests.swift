// CoreML and the .mlpackage fixtures are macOS-only; the suite compiles away
// on Linux so cross-compilation and Linux CI remain unaffected.
#if os(macOS)

import Foundation
import Testing

@testable import LocalInference

/// Startup-time output-schema validation (FR-003, FR-004): CoreMLEmbeddingActor.init
/// must throw distinct, named errors for each kind of model-spec mismatch, instead of
/// the legacy `embeddingModelNotLoaded` catch-all (which is now reserved for the true
/// not-loaded condition, surfaced as a 503 at the router).
@Suite("CoreMLEmbeddingActor — output schema validation")
struct EmbeddingOutputValidationTests {
    private static func fixtureURL(_ name: String) throws -> URL {
        guard let resourceRoot = Bundle.module.resourceURL else {
            throw FixtureError.missingResourceBundle
        }
        return resourceRoot
            .appendingPathComponent("Fixtures")
            .appendingPathComponent(name)
    }

    private static func makeActor(fixture: String) async throws -> CoreMLEmbeddingActor {
        let modelURL = try fixtureURL(fixture)
        let cachePath = try fixtureURL("tokenizer-cache").path
        return try await CoreMLEmbeddingActor(
            modelURL: modelURL,
            tokenizerModelId: "BAAI/bge-base-en-v1.5",
            hubCachePath: cachePath
        )
    }

    @Test func unsupportedOutputDtypeThrowsSpecificError() async throws {
        await #expect(throws: LocalInferenceError.self) {
            _ = try await Self.makeActor(fixture: "stub-bge-base-bad-dtype.mlpackage")
        }

        do {
            _ = try await Self.makeActor(fixture: "stub-bge-base-bad-dtype.mlpackage")
            Issue.record("expected init to throw embeddingOutputDtypeUnsupported")
        } catch LocalInferenceError.embeddingOutputDtypeUnsupported(let observed, let supported) {
            #expect(observed == "int32", "observed dtype should name the actual fixture dtype")
            #expect(supported.contains("float32"), "supported set must mention float32")
            #expect(supported.contains("float16"), "supported set must mention float16")
        } catch {
            Issue.record("expected embeddingOutputDtypeUnsupported, got \(error)")
        }
    }

    @Test func wrongOutputShapeThrowsSpecificError() async throws {
        do {
            _ = try await Self.makeActor(fixture: "stub-bge-base-bad-shape.mlpackage")
            Issue.record("expected init to throw embeddingOutputShapeMismatch")
        } catch LocalInferenceError.embeddingOutputShapeMismatch(let observed, let expected) {
            #expect(expected == [1, 512, 768], "expected shape must match the bge-base hidden state contract")
            #expect(observed != expected, "observed shape must differ from expected")
        } catch {
            Issue.record("expected embeddingOutputShapeMismatch, got \(error)")
        }
    }

    @Test func missingOutputThrowsSpecificError() async throws {
        do {
            _ = try await Self.makeActor(fixture: "stub-bge-base-bad-output-name.mlpackage")
            Issue.record("expected init to throw embeddingOutputMissing")
        } catch LocalInferenceError.embeddingOutputMissing(let name) {
            #expect(name == "last_hidden_state", "missing-name error must name the contract feature")
        } catch {
            Issue.record("expected embeddingOutputMissing, got \(error)")
        }
    }
}

private enum FixtureError: Error {
    case missingResourceBundle
}

#endif
