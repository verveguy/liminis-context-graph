import Foundation

// MARK: - OpenAI-compatible request/response types

struct ChatMessage: Codable, Sendable {
    let role: String
    let content: String
}

struct ResponseFormat: Codable, Sendable {
    let type: String
}

struct ChatCompletionRequest: Codable, Sendable {
    let model: String
    let messages: [ChatMessage]
    let temperature: Double?
    let maxTokens: Int?
    let responseFormat: ResponseFormat?
    let stream: Bool?

    enum CodingKeys: String, CodingKey {
        case model, messages, temperature, stream
        case maxTokens = "max_tokens"
        case responseFormat = "response_format"
    }
}

struct ChatCompletionChoice: Codable, Sendable {
    let index: Int
    let message: ChatMessage
    let finishReason: String

    enum CodingKeys: String, CodingKey {
        case index, message
        case finishReason = "finish_reason"
    }
}

struct ChatCompletionUsage: Codable, Sendable {
    let promptTokens: Int
    let completionTokens: Int
    let totalTokens: Int

    enum CodingKeys: String, CodingKey {
        case promptTokens = "prompt_tokens"
        case completionTokens = "completion_tokens"
        case totalTokens = "total_tokens"
    }
}

struct ChatCompletionResponse: Codable, Sendable {
    let id: String
    let object: String
    let created: Int
    let model: String
    let choices: [ChatCompletionChoice]
    let usage: ChatCompletionUsage
}

// MARK: - Streaming SSE types

struct StreamDelta: Codable, Sendable {
    let role: String?
    let content: String?
}

struct StreamChoice: Codable, Sendable {
    let index: Int
    let delta: StreamDelta
    let finishReason: String?

    enum CodingKeys: String, CodingKey {
        case index, delta
        case finishReason = "finish_reason"
    }
}

struct StreamChunk: Codable, Sendable {
    let id: String
    let object: String
    let created: Int
    let model: String
    let choices: [StreamChoice]
}

// MARK: - Error response

struct ErrorDetail: Codable, Sendable {
    let message: String
    let type: String
    let code: String?
}

struct ErrorResponse: Codable, Sendable {
    let error: ErrorDetail
}
