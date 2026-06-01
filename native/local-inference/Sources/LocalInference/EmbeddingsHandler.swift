import Accelerate
import Accelerate.vImage
import CoreML
import Foundation
import Hub
import Hummingbird
import Tokenizers

// MARK: - CoreML Embedding Actor

/// Serializes all CoreML embedding calls through a single actor.
///
/// Thread-safety note: Apple's CoreML documentation states that MLModel.prediction()
/// is thread-safe. The actor pattern is retained for consistency with the codebase
/// (the NLEmbedding predecessor required serialization to prevent SIGTRAP/SIGSEGV)
/// and to guard against undocumented edge cases in ANE scheduling.
///
/// The model produces last_hidden_state (1, 512, 768). CLS pooling and L2
/// normalization are applied here in Swift (REQ-03 fallback path: baked-in graph
/// pooling was not feasible with the torch.jit.trace + coremltools 8.1 toolchain).
///
/// Batch handling: the exported .mlpackage has a fixed batch dimension of 1.
/// Batch requests are processed as N sequential prediction() calls inside the actor.
/// A single HTTP request still gets a single actor invocation — no per-text IPC.
actor CoreMLEmbeddingActor {
    static let maxSequenceLength = 512
    static let dimension = 768
    static let outputFeatureName = "last_hidden_state"

    /// Single source of truth for the CoreML output dtypes the handler accepts.
    /// Read by both `validateOutputSchema` (startup) and `embed(texts:)` (per-request dispatch).
    /// Adding a new supported dtype means extending this set AND the dispatch in `embed`.
    static let supportedOutputDataTypes: Set<MLMultiArrayDataType> = [.float32, .float16]

    private let model: MLModel
    private let tokenizer: any Tokenizer

    /// Human-readable name for an `MLMultiArrayDataType`, used in startup error messages.
    static func dtypeName(_ dtype: MLMultiArrayDataType) -> String {
        switch dtype {
        case .float16: return "float16"
        case .float32: return "float32"
        case .float64: return "float64"
        case .int32:   return "int32"
        default: return "unknown(\(dtype.rawValue))"
        }
    }

    /// Validate the loaded model exposes a `last_hidden_state` output with a supported dtype
    /// and the expected `[1, maxSequenceLength, dimension]` shape. Runs once at startup.
    ///
    /// Distinct error cases are thrown so the router (and tests) can produce specific
    /// messages instead of the legacy "model not loaded" catch-all.
    static func validateOutputSchema(_ model: MLModel) throws {
        guard let outputDesc = model.modelDescription.outputDescriptionsByName[outputFeatureName] else {
            throw LocalInferenceError.embeddingOutputMissing(name: outputFeatureName)
        }

        guard let constraint = outputDesc.multiArrayConstraint else {
            // Output exists but is not an MLMultiArray (e.g. an image or string feature).
            throw LocalInferenceError.embeddingOutputDtypeUnsupported(
                observed: "non-multi-array",
                supported: supportedOutputDataTypes.map { dtypeName($0) }.sorted()
            )
        }

        let observedShape = constraint.shape.map { $0.intValue }
        let expectedShape = [1, maxSequenceLength, dimension]
        if observedShape != expectedShape {
            throw LocalInferenceError.embeddingOutputShapeMismatch(
                observed: observedShape,
                expected: expectedShape
            )
        }

        if !supportedOutputDataTypes.contains(constraint.dataType) {
            throw LocalInferenceError.embeddingOutputDtypeUnsupported(
                observed: dtypeName(constraint.dataType),
                supported: supportedOutputDataTypes.map { dtypeName($0) }.sorted()
            )
        }
    }

    /// Load the CoreML model and tokenizer. Runs at sidecar startup (main.swift).
    ///
    /// - Parameters:
    ///   - modelURL: Path to .mlpackage (compiled on first run) or .mlmodelc.
    ///   - tokenizerModelId: HuggingFace model ID, e.g. "BAAI/bge-base-en-v1.5".
    ///   - hubCachePath: Optional path to a local HuggingFace Hub cache directory.
    ///     When set, the tokenizer is loaded offline from that cache.
    ///   - mlmodelcCacheDir: Optional per-device cache directory for the compiled
    ///     `.mlmodelc`. When set and `modelURL` points at a `.mlpackage`, the actor
    ///     compiles once on first launch, installs the result into the cache (atomic
    ///     rename + sentinel), and loads the cached copy on subsequent launches.
    ///     Progress is emitted to stderr with the `[setup]` prefix.
    init(
        modelURL: URL,
        tokenizerModelId: String,
        hubCachePath: String? = nil,
        mlmodelcCacheDir: URL? = nil
    ) async throws {
        let config = MLModelConfiguration()
        config.computeUnits = .cpuAndNeuralEngine

        let loadURL = try await Self.resolveLoadURL(
            modelURL: modelURL,
            mlmodelcCacheDir: mlmodelcCacheDir
        )

        // Async model load (available macOS 12+; we require macOS 26)
        self.model = try await MLModel.load(contentsOf: loadURL, configuration: config)

        // Fail-fast at startup if the bundled model's output schema doesn't match
        // what the handler can consume. Distinct error cases let the router (and
        // SetupWizard) name the actual problem instead of throwing a misleading
        // "model not loaded" at the first /v1/embeddings request.
        try Self.validateOutputSchema(self.model)

        // Load tokenizer from HuggingFace Hub (online) or local cache (offline).
        // When hubCachePath is set, configure HubApi to use that directory as the
        // downloadBase (structure: <cachePath>/models/<org>/<model>/).
        let hub: HubApi
        if let cachePath = hubCachePath {
            hub = HubApi(downloadBase: URL(fileURLWithPath: cachePath), useOfflineMode: true)
        } else {
            hub = .shared
        }
        self.tokenizer = try await AutoTokenizer.from(pretrained: tokenizerModelId, hubApi: hub)

        // Log compute unit configuration. Actual ANE vs CPU fallback is determined at
        // runtime by CoreML based on model compatibility and device state.
        print("local-inference: CoreML embedding model loaded from \(modelURL.lastPathComponent)")
        print("local-inference: Embedding compute units: .cpuAndNeuralEngine (ANE if available)")
        print("local-inference: Embedding tokenizer: \(tokenizerModelId)\(hubCachePath != nil ? " (local cache)" : "")")
    }

    /// Resolve the URL CoreML should load from, performing first-launch compile-and-cache
    /// when the caller supplies a cache directory and the source is a `.mlpackage`.
    ///
    /// Behavior:
    /// - `mlmodelcCacheDir == nil` and source is `.mlpackage` → compile to temp directory
    ///   (legacy dev path; no persistence, compile cost repeated every launch).
    /// - `mlmodelcCacheDir == nil` and source is `.mlmodelc` → return source URL.
    /// - `mlmodelcCacheDir != nil` and source is `.mlmodelc` → return source URL
    ///   (caller already has a compiled model; cache directory is unused).
    /// - `mlmodelcCacheDir != nil` and source is `.mlpackage` → run first-launch flow:
    ///   check sentinel + hash → load cached copy on hit, or compile → install → sentinel.
    ///   Emits `[setup]` progress to stderr.
    static func resolveLoadURL(modelURL: URL, mlmodelcCacheDir: URL?) async throws -> URL {
        let isPackage = modelURL.pathExtension == "mlpackage"

        guard let cacheDir = mlmodelcCacheDir, isPackage else {
            if isPackage {
                let compiledURL = try await MLModel.compileModel(at: modelURL)
                print("local-inference: Compiled .mlpackage → \(compiledURL.lastPathComponent)")
                return compiledURL
            }
            return modelURL
        }

        try SetupCache.ensureCacheDir(cacheDir)

        let expectedHash = SetupCache.buildHash(for: modelURL)
        if SetupCache.cacheIsValid(cacheDir: cacheDir, expectedHash: expectedHash) {
            let cachedURL = SetupCache.compiledModelURL(in: cacheDir)
            FileHandle.standardError.emitSetupEvent(stage: "model_ready", message: "cached")
            return cachedURL
        }

        // Either a partial/stale cache from a crashed compile, or a fresh cache, or
        // a hash mismatch from a model upgrade. Sweep before recompiling.
        SetupCache.cleanStaleCache(cacheDir: cacheDir)

        FileHandle.standardError.emitSetupEvent(stage: "compiling_model", message: "first-launch compile")
        let compiledTempURL = try await MLModel.compileModel(at: modelURL)

        FileHandle.standardError.emitSetupEvent(stage: "installing_model", message: "writing to cache")
        let installedURL = try SetupCache.installCompiledModel(from: compiledTempURL, into: cacheDir)

        FileHandle.standardError.emitSetupEvent(stage: "verifying_model")
        _ = try await MLModel.load(contentsOf: installedURL, configuration: MLModelConfiguration())

        // A racing sidecar may have written the sentinel already (O_EXCL → EEXIST);
        // since the model on disk is verified, treat that as success rather than
        // aborting init.
        do {
            try SetupCache.writeSentinel(at: SetupCache.sentinelURL(in: cacheDir), buildHash: expectedHash)
        } catch SetupCache.SetupCacheError.sentinelWriteRace {
            // Another process completed setup concurrently — proceed.
        }
        FileHandle.standardError.emitSetupEvent(stage: "model_ready", message: "compiled")
        return installedURL
    }

    /// Embed a batch of texts. Returns one EmbeddingData per input (sequential calls,
    /// fixed batch=1 MLModel). Empty strings return a zero vector (REQ-12).
    func embed(texts: [String]) throws -> [EmbeddingData] {
        var results: [EmbeddingData] = []

        for (index, text) in texts.enumerated() {
            // Empty string → zero vector (REQ-12)
            guard !text.isEmpty else {
                results.append(EmbeddingData(
                    object: "embedding",
                    embedding: Array(repeating: 0.0, count: Self.dimension),
                    index: index
                ))
                continue
            }

            // encode() adds [CLS]=101 at position 0 and [SEP]=102 at the end.
            var inputIds = tokenizer.encode(text: text)

            // Truncate to maxSequenceLength (keep [CLS] at 0, replace last token with [SEP]=102)
            let sepTokenId = 102
            let padTokenId = 0
            if inputIds.count > Self.maxSequenceLength {
                inputIds = Array(inputIds.prefix(Self.maxSequenceLength - 1)) + [sepTokenId]
            }

            let tokenCount = inputIds.count
            let padded = inputIds + Array(repeating: padTokenId, count: Self.maxSequenceLength - tokenCount)
            let attentionMask = Array(repeating: 1, count: tokenCount)
                + Array(repeating: 0, count: Self.maxSequenceLength - tokenCount)
            let tokenTypeIds = Array(repeating: 0, count: Self.maxSequenceLength)

            // Build MLMultiArray inputs (batch=1, seq=512).
            // Fill via direct pointer access — NSNumber subscript bridging has significant
            // overhead in a tight loop over 512 elements per input array.
            let shape: [NSNumber] = [1, NSNumber(value: Self.maxSequenceLength)]
            let idsArray    = try MLMultiArray(shape: shape, dataType: .int32)
            let maskArray   = try MLMultiArray(shape: shape, dataType: .int32)
            let typesArray  = try MLMultiArray(shape: shape, dataType: .int32)

            let idsPtr   = idsArray.dataPointer.bindMemory(to: Int32.self, capacity: Self.maxSequenceLength)
            let maskPtr  = maskArray.dataPointer.bindMemory(to: Int32.self, capacity: Self.maxSequenceLength)
            let typesPtr = typesArray.dataPointer.bindMemory(to: Int32.self, capacity: Self.maxSequenceLength)

            for i in 0..<Self.maxSequenceLength {
                idsPtr[i]   = Int32(padded[i])
                maskPtr[i]  = Int32(attentionMask[i])
                typesPtr[i] = Int32(tokenTypeIds[i])
            }

            let featureDict: [String: MLFeatureValue] = [
                "input_ids":      MLFeatureValue(multiArray: idsArray),
                "attention_mask": MLFeatureValue(multiArray: maskArray),
                "token_type_ids": MLFeatureValue(multiArray: typesArray),
            ]
            let features = try MLDictionaryFeatureProvider(dictionary: featureDict)
            let output = try model.prediction(from: features)

            // CLS pooling: take hidden state at position 0 → shape (768,).
            // Use direct pointer access for the CLS vector — element [0,0,0..767] is
            // contiguous at the start of the flat buffer for shape [1,512,768].
            // Startup-time validateOutputSchema guarantees the output exists with a
            // supported dtype and the expected shape, so this is a sanity check, not
            // a schema gate. If the output disappears between startup and a request
            // (e.g. the model file is swapped under us), throw the schema-specific
            // error rather than the nil-actor sentinel — the router already maps
            // embeddingOutputMissing to a 500 naming the missing feature.
            guard let hiddenState = output.featureValue(for: Self.outputFeatureName)?.multiArrayValue else {
                throw LocalInferenceError.embeddingOutputMissing(name: Self.outputFeatureName)
            }

            var clsVector = Self.extractCLSVector(from: hiddenState)

            // L2 normalization via Accelerate (vectorized)
            var norm: Float = 0
            vDSP_svesq(clsVector, 1, &norm, vDSP_Length(Self.dimension))
            norm = sqrtf(norm)
            if norm > 0 {
                vDSP_vsdiv(clsVector, 1, &norm, &clsVector, 1, vDSP_Length(Self.dimension))
            }

            results.append(EmbeddingData(
                object: "embedding",
                embedding: clsVector.map { Double($0) },
                index: index
            ))
        }

        return results
    }

    /// Extract the CLS vector (first 768 elements at hiddenState[0,0,:]) as `[Float]`,
    /// converting from the model's native dtype when needed.
    ///
    /// CLS pooling discards the other 511 tokens, so we only ever touch the leading
    /// 768 elements regardless of source dtype. Half→Float conversion uses vImage's
    /// vectorized `vImageConvert_Planar16FtoPlanarF` (sub-microsecond for 768 elts).
    ///
    /// Startup-time `validateOutputSchema` guarantees `hiddenState.dataType` is in
    /// `supportedOutputDataTypes`. The `default` case is unreachable in production
    /// but kept defensive in case the registry is extended without updating dispatch.
    static func extractCLSVector(from hiddenState: MLMultiArray) -> [Float] {
        switch hiddenState.dataType {
        case .float32:
            let floatPtr = hiddenState.dataPointer.bindMemory(to: Float.self, capacity: hiddenState.count)
            return Array(UnsafeBufferPointer(start: floatPtr, count: dimension))

        case .float16:
            var result = [Float](repeating: 0, count: dimension)
            var srcBuf = vImage_Buffer(
                data: hiddenState.dataPointer,
                height: 1,
                width: vImagePixelCount(dimension),
                rowBytes: dimension * MemoryLayout<UInt16>.size
            )
            let status = result.withUnsafeMutableBufferPointer { dstPtr -> vImage_Error in
                var dstBuf = vImage_Buffer(
                    data: dstPtr.baseAddress!,
                    height: 1,
                    width: vImagePixelCount(dimension),
                    rowBytes: dimension * MemoryLayout<Float>.size
                )
                return vImageConvert_Planar16FtoPlanarF(&srcBuf, &dstBuf, 0)
            }
            // Fail loudly on conversion error rather than returning the pre-zeroed
            // buffer, which would silently produce a degenerate embedding.
            precondition(
                status == kvImageNoError,
                "vImageConvert_Planar16FtoPlanarF failed with status \(status)"
            )
            return result

        default:
            // Unreachable when validateOutputSchema is in effect. Fail loudly
            // instead of returning zeros so any future drift between
            // supportedOutputDataTypes and this dispatch surfaces immediately.
            preconditionFailure(
                "extractCLSVector: unhandled dtype \(dtypeName(hiddenState.dataType)) — " +
                "supportedOutputDataTypes was extended without updating dispatch"
            )
        }
    }
}

// MARK: - Handler

func handleEmbeddings(
    request: Request,
    context: BasicRequestContext,
    actor: CoreMLEmbeddingActor
) async throws -> Response {
    let body: EmbeddingRequest
    do {
        body = try await request.decode(as: EmbeddingRequest.self, context: context)
    } catch is DecodingError {
        return makeErrorResponse(status: .badRequest, message: "Invalid embedding request body")
    }
    let texts = body.input.texts

    guard !texts.isEmpty else {
        return makeErrorResponse(status: .badRequest, message: "input must not be empty")
    }

    let results = try await actor.embed(texts: texts)
    let totalTokenEstimate = texts.reduce(0) { $0 + max(1, $1.split(separator: " ").count) }

    let response = EmbeddingResponse(
        object: "list",
        data: results,
        model: "coreml-bge-base-en-v1.5",
        usage: EmbeddingUsage(
            promptTokens: totalTokenEstimate,
            totalTokens: totalTokenEstimate
        )
    )

    let encoder = JSONEncoder()
    let jsonData = try encoder.encode(response)
    return Response(
        status: .ok,
        headers: [.contentType: "application/json"],
        body: .init(byteBuffer: .init(data: jsonData))
    )
}
