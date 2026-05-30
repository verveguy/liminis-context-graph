import Foundation
import Hummingbird

private let encoder: JSONEncoder = {
    let e = JSONEncoder()
    e.outputFormatting = .sortedKeys
    return e
}()

// MARK: - Non-streaming handler

func handleChatCompletions(
    request: Request,
    context: some RequestContext,
    adapter: some InferenceAdapter
) async throws -> Response {
    let body: ChatCompletionRequest
    do {
        body = try await request.decode(as: ChatCompletionRequest.self, context: context)
    } catch {
        return makeErrorResponse(status: .badRequest, message: "Invalid request body: could not decode the request.")
    }

    if body.stream == true {
        // checkAvailability() must be called here, before committing the 200 OK SSE headers.
        // Errors thrown inside ResponseBody.stream happen after headers are sent and can't become 503.
        try adapter.checkAvailability()
        return try await streamingResponse(for: body, adapter: adapter)
    } else {
        return try await blockingResponse(for: body, adapter: adapter)
    }
}

// MARK: - Blocking response

func blockingResponse(
    for request: ChatCompletionRequest,
    adapter: some InferenceAdapter
) async throws -> Response {
    let content: String
    do {
        var raw = try await adapter.respond(to: request)
        if request.responseFormat?.type == "json_object" {
            raw = extractJSON(from: raw)
        }
        content = raw
    } catch let err as LocalInferenceError {
        // Let LocalInferenceErrors propagate so the router can map them to the correct HTTP status.
        throw err
    } catch {
        let messageCount = request.messages.count
        let totalChars = request.messages.map(\.content).reduce(0) { $0 + $1.count }
        fputs("[local-inference] Chat completion error: \(error) (messages: \(messageCount), chars: \(totalChars))\n", stderr)
        return makeErrorResponse(status: .internalServerError, message: "Inference failed")
    }

    let messageTokens = estimateTokens(request.messages.map(\.content).joined())
    let completionTokens = estimateTokens(content)

    let response = ChatCompletionResponse(
        id: "chatcmpl-\(UUID().uuidString)",
        object: "chat.completion",
        created: Int(Date().timeIntervalSince1970),
        model: request.model,
        choices: [
            ChatCompletionChoice(
                index: 0,
                message: ChatMessage(role: "assistant", content: content),
                finishReason: "stop"
            )
        ],
        usage: ChatCompletionUsage(
            promptTokens: messageTokens,
            completionTokens: completionTokens,
            totalTokens: messageTokens + completionTokens
        )
    )

    let data = try encoder.encode(response)
    return Response(
        status: .ok,
        headers: [.contentType: "application/json"],
        body: .init(byteBuffer: .init(data: data))
    )
}

// MARK: - Streaming SSE response

func streamingResponse(
    for request: ChatCompletionRequest,
    adapter: some InferenceAdapter
) async throws -> Response {
    let completionID = "chatcmpl-\(UUID().uuidString)"
    let created = Int(Date().timeIntervalSince1970)
    let model = request.model

    let stream = adapter.stream(request: request)

    let responseBody = ResponseBody { writer in
        let roleChunk = StreamChunk(
            id: completionID,
            object: "chat.completion.chunk",
            created: created,
            model: model,
            choices: [StreamChoice(index: 0, delta: StreamDelta(role: "assistant", content: nil), finishReason: nil)]
        )
        try await sendSSEChunk(roleChunk, to: &writer)

        for try await text in stream {
            let chunk = StreamChunk(
                id: completionID,
                object: "chat.completion.chunk",
                created: created,
                model: model,
                choices: [StreamChoice(index: 0, delta: StreamDelta(role: nil, content: text), finishReason: nil)]
            )
            try await sendSSEChunk(chunk, to: &writer)
        }

        let stopChunk = StreamChunk(
            id: completionID,
            object: "chat.completion.chunk",
            created: created,
            model: model,
            choices: [StreamChoice(index: 0, delta: StreamDelta(role: nil, content: nil), finishReason: "stop")]
        )
        try await sendSSEChunk(stopChunk, to: &writer)

        try await writer.write(ByteBuffer(string: "data: [DONE]\n\n"))
        try await writer.finish(nil)
    }

    return Response(
        status: .ok,
        headers: [
            .contentType: "text/event-stream",
            .cacheControl: "no-cache",
            .connection: "keep-alive",
        ],
        body: responseBody
    )
}

// ResponseBodyWriter.write/_finish are mutating/consuming — helper must take inout.
private func sendSSEChunk(_ chunk: StreamChunk, to writer: inout any ResponseBodyWriter) async throws {
    let data = try encoder.encode(chunk)
    guard let json = String(data: data, encoding: .utf8) else {
        throw HTTPError(.internalServerError, message: "Failed to encode SSE chunk to UTF-8")
    }
    try await writer.write(ByteBuffer(string: "data: \(json)\n\n"))
}

// MARK: - Health check

func handleHealth(request: Request, context: some RequestContext) async throws -> Response {
    let body = ByteBuffer(string: #"{"status":"ok","model":"apple-foundation"}"#)
    return Response(
        status: .ok,
        headers: [.contentType: "application/json"],
        body: .init(byteBuffer: body)
    )
}

// MARK: - Error response

func makeErrorResponse(status: HTTPResponse.Status, message: String) -> Response {
    let errorType = status.code >= 400 && status.code < 500 ? "invalid_request_error" : "server_error"
    let errDetail = ErrorDetail(message: message, type: errorType, code: nil)
    let errResponse = ErrorResponse(error: errDetail)
    let data: Data
    do {
        data = try encoder.encode(errResponse)
    } catch {
        let raw = #"{"error":{"message":"Failed to encode error response.","type":"server_error","code":null}}"#
        data = Data(raw.utf8)
    }
    return Response(
        status: status,
        headers: [.contentType: "application/json"],
        body: .init(byteBuffer: .init(data: data))
    )
}

// MARK: - Token estimation

private func estimateTokens(_ text: String) -> Int {
    max(1, text.count / 4)
}
