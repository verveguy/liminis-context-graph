import Foundation

/// Which capability set the sidecar serves, selected via the `LOCAL_INFERENCE_MODE`
/// environment variable. Defaults to `.both`, matching the sidecar's pre-existing behavior.
enum LocalInferenceMode: String, Sendable {
    case embeddings
    case completions
    case both

    static let envVarName = "LOCAL_INFERENCE_MODE"

    /// Thrown by `parse(_:)` when `LOCAL_INFERENCE_MODE` is set to an unrecognized value.
    enum ParseError: Error, Sendable, Equatable {
        case invalidValue(String)
    }

    /// Parses the raw `LOCAL_INFERENCE_MODE` value. `nil` (the env var unset) defaults to `.both`.
    /// Throws `ParseError.invalidValue` for any non-`nil` string that isn't `embeddings`,
    /// `completions`, or `both`.
    static func parse(_ raw: String?) throws -> LocalInferenceMode {
        guard let raw else { return .both }
        guard let mode = LocalInferenceMode(rawValue: raw) else {
            throw ParseError.invalidValue(raw)
        }
        return mode
    }

    var includesEmbeddings: Bool {
        switch self {
        case .embeddings, .both: return true
        case .completions: return false
        }
    }

    var includesCompletions: Bool {
        switch self {
        case .completions, .both: return true
        case .embeddings: return false
        }
    }
}
