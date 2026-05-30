import Foundation

// MARK: - OpenAI-compatible embedding request/response types

struct EmbeddingRequest: Codable, Sendable {
    let input: EmbeddingInput
    let model: String?

    enum EmbeddingInput: Codable, Sendable {
        case single(String)
        case batch([String])

        init(from decoder: any Decoder) throws {
            let container = try decoder.singleValueContainer()
            if let text = try? container.decode(String.self) {
                self = .single(text)
            } else {
                self = .batch(try container.decode([String].self))
            }
        }

        func encode(to encoder: any Encoder) throws {
            var container = encoder.singleValueContainer()
            switch self {
            case .single(let text): try container.encode(text)
            case .batch(let texts): try container.encode(texts)
            }
        }

        var texts: [String] {
            switch self {
            case .single(let text): return [text]
            case .batch(let texts): return texts
            }
        }
    }
}

struct EmbeddingData: Codable, Sendable {
    let object: String
    let embedding: [Double]
    let index: Int
}

struct EmbeddingUsage: Codable, Sendable {
    let promptTokens: Int
    let totalTokens: Int

    enum CodingKeys: String, CodingKey {
        case promptTokens = "prompt_tokens"
        case totalTokens = "total_tokens"
    }
}

struct EmbeddingResponse: Codable, Sendable {
    let object: String
    let data: [EmbeddingData]
    let model: String
    let usage: EmbeddingUsage
}
