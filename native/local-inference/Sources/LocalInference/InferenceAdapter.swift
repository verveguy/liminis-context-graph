import Foundation

/// Abstraction over the model backend. Production code uses `FoundationModelsAdapter`;
/// tests inject a `MockInferenceAdapter` that returns deterministic strings.
protocol InferenceAdapter: Sendable {
    /// Throws `LocalInferenceError.foundationModelsUnavailable` if the model backend is not ready.
    /// Must be called before `stream()` so the handler can return 503 before committing response headers.
    func checkAvailability() throws
    func respond(to request: ChatCompletionRequest) async throws -> String
    func stream(request: ChatCompletionRequest) -> AsyncThrowingStream<String, any Error>
}

/// Best-effort extraction of the first JSON object from a model response that
/// may include surrounding prose or markdown code fences.
///
/// Extracted as a top-level function (not private) so it can be unit-tested directly.
func extractJSON(from text: String) -> String {
    var cleaned = text

    // Strip ```json ... ``` fences
    if let start = cleaned.range(of: "```json"),
       let end = cleaned.range(of: "```", range: start.upperBound..<cleaned.endIndex) {
        cleaned = String(cleaned[start.upperBound..<end.lowerBound])
            .trimmingCharacters(in: .whitespacesAndNewlines)
    } else if let start = cleaned.range(of: "```"),
              let end = cleaned.range(of: "```", range: start.upperBound..<cleaned.endIndex) {
        cleaned = String(cleaned[start.upperBound..<end.lowerBound])
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    // Find outermost { … } span. If the closing brace appears before the opening
    // one (e.g. model output containing prose between dangling braces), there is no
    // valid JSON span to extract — return the cleaned text and let downstream parsing
    // surface a clear error rather than crashing on an inverted range.
    guard let firstBrace = cleaned.firstIndex(of: "{"),
          let lastBrace  = cleaned.lastIndex(of: "}"),
          firstBrace <= lastBrace else {
        return cleaned
    }
    return String(cleaned[firstBrace...lastBrace])
}
