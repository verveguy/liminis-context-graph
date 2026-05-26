// Corpus copied verbatim from:
//   liminis-app/native/local-inference/benchmark/bench.py  (200-sentence bench set)
//   liminis-app/native/local-inference/verify-embedding-parity.py  (50-sentence parity set)

const SHORT: &[&str] = &[
    "Hello world.",
    "The cat sat on the mat.",
    "Swift is fast.",
    "Apple Silicon is powerful.",
    "Machine learning is everywhere.",
    "Open source software matters.",
    "The sun rises in the east.",
    "Knowledge graphs connect information.",
    "Embeddings encode semantics.",
    "CoreML runs on the Neural Engine.",
    "Paris is the capital of France.",
    "Water boils at 100 degrees Celsius.",
    "The Eiffel Tower is in Paris.",
    "Transformers changed NLP forever.",
    "BERT uses bidirectional attention.",
    "Vector search enables semantic retrieval.",
    "CLS pooling extracts sentence embeddings.",
    "L2 normalization ensures unit vectors.",
    "The Accelerate framework is vectorized.",
    "Unix domain sockets have low latency.",
];

const MEDIUM: &[&str] = &[
    "The quick brown fox jumps over the lazy dog, demonstrating every letter of the alphabet.",
    "Artificial intelligence is transforming industries from healthcare to finance and beyond.",
    "The Hummingbird framework provides a lightweight HTTP server for Swift applications.",
    "CoreML allows developers to integrate machine learning models into their Apple platform apps.",
    "Semantic search retrieves documents based on meaning rather than exact keyword matches.",
    "The BERT model was pre-trained on a large corpus using masked language modeling objectives.",
    "Apple's Neural Engine accelerates machine learning inference with dedicated hardware.",
    "Graph databases represent relationships between entities as first-class citizens.",
    "The Graphiti library builds knowledge graphs with temporal awareness and entity resolution.",
    "Swift's actor model provides structured concurrency and data-race safety at compile time.",
    "FalkorDB is a graph database built on Redis, optimized for Cypher queries.",
    "Tokenization converts raw text into integer token IDs for transformer model input.",
    "Attention mechanisms allow models to weigh the importance of different input tokens.",
    "Fine-tuning a pre-trained language model requires significantly less data than training from scratch.",
    "The CLS token in BERT aggregates information from the entire input sequence.",
    "Cosine similarity measures the angle between two vectors, ignoring magnitude differences.",
    "Batch processing amortizes model initialization overhead across multiple requests.",
    "The sentence-transformers library provides pre-trained models for semantic embeddings.",
    "MLMultiArray is the CoreML data structure for passing tensor inputs and outputs.",
    "Swift's value types and reference types have different ownership and mutation semantics.",
    "ANE scheduling depends on model architecture, compute graph, and device thermal state.",
    "Hummingbird's router supports type-safe path parameters and middleware composition.",
    "The attention_mask tensor tells BERT which tokens to attend to and which to ignore.",
    "Quantization reduces model size by representing weights with fewer bits of precision.",
    "The Faiss library provides efficient similarity search for high-dimensional vectors.",
    "MLModel.prediction() is documented as thread-safe in Apple's CoreML documentation.",
    "The swift-transformers library provides tokenizer implementations for Swift applications.",
    "Padding ensures all sequences in a batch have the same length for tensor operations.",
    "The [SEP] token marks the boundary between sentence pairs in BERT inputs.",
    "Truncation prevents sequences longer than 512 tokens from exceeding the model's context.",
];

const LONG: &[&str] = &[
    "Knowledge graphs are structured representations of information that capture entities and their \
     relationships in a graph format. They have been used extensively in search engines, question \
     answering systems, and recommendation engines. Recent work has explored combining knowledge \
     graphs with large language models to improve factual accuracy and enable multi-hop reasoning.",
    "The transformer architecture, introduced in the paper 'Attention Is All You Need' by Vaswani \
     et al. in 2017, replaced recurrent neural networks with self-attention mechanisms. This allowed \
     for much more efficient parallel training and better capture of long-range dependencies in text. \
     BERT, GPT, and their successors are all built on this foundational architecture.",
    "Apple Silicon chips, including the M1, M2, M3, and M4 families, integrate CPU, GPU, Neural \
     Engine, and memory on a single die. This unified memory architecture eliminates the need to \
     copy data between separate memory pools, reducing latency for machine learning inference. The \
     Neural Engine supports INT8 and FP16 operations optimized for transformer attention patterns.",
    "CoreML Tools is a Python library for converting models from frameworks like PyTorch, \
     TensorFlow, and scikit-learn into the CoreML format. The conversion process involves tracing \
     or scripting the model to capture its computational graph, then converting operations to \
     CoreML's intermediate representation. The resulting .mlpackage can be integrated directly \
     into macOS, iOS, and other Apple platform applications.",
    "Semantic search systems typically involve encoding queries and documents into a shared \
     embedding space, then retrieving the nearest neighbors by cosine similarity or inner product. \
     The quality of these embeddings determines recall and precision. Models like BGE (BAAI General \
     Embedding) are specifically optimized for retrieval tasks using contrastive learning objectives \
     that pull similar pairs together and push dissimilar pairs apart in embedding space.",
    "The Swift programming language was designed by Apple to be safe, fast, and expressive. Its \
     type system prevents common programming errors like null pointer dereferences, and its \
     performance characteristics make it suitable for systems programming. Swift 6 introduced \
     strict concurrency checking, making data races compile-time errors rather than runtime bugs. \
     This has significant implications for server-side Swift development and actor-based APIs.",
    "Graph neural networks (GNNs) extend deep learning to graph-structured data by performing \
     iterative message passing between nodes and their neighbors. Applications include drug \
     discovery, where molecular graphs represent chemical compounds, social network analysis, \
     and knowledge graph completion. GNNs can also be combined with transformer architectures \
     in hybrid models that leverage both local graph structure and global attention.",
    "The Unix domain socket is an inter-process communication mechanism that allows processes \
     on the same host to communicate through the filesystem namespace. Unlike TCP sockets, \
     Unix domain sockets skip the network stack entirely, providing lower latency and higher \
     throughput for local IPC. They are commonly used for database connections, service meshes, \
     and other local service communication patterns where network overhead is undesirable.",
    "Retrieval-augmented generation (RAG) combines information retrieval with language model \
     generation. A retriever fetches relevant context from a corpus based on the query, and a \
     generator conditions its output on both the query and the retrieved context. This approach \
     allows language models to access up-to-date information without expensive fine-tuning, and \
     provides a mechanism for attribution since the retrieved sources can be shown to users.",
    "L2 normalization maps vectors to the unit hypersphere, making cosine similarity equivalent \
     to dot product similarity. This simplification is important for efficient approximate nearest \
     neighbor search using algorithms like HNSW or IVF, which are optimized for inner product \
     computations. Apple's Accelerate framework provides vectorized BLAS and vDSP routines that \
     can compute L2 norms and dot products orders of magnitude faster than naive Python loops.",
    "The Graphiti framework provides temporal knowledge graph capabilities, tracking when facts \
     were added and modified. This temporal dimension is crucial for applications that need to \
     reason about changing states of the world, such as tracking the evolution of a codebase, \
     monitoring project status, or understanding how relationships between people and organizations \
     change over time. Graphiti integrates with FalkorDB for persistent graph storage and retrieval.",
    "Spike-based development, or time-boxed technical exploration, is a common practice in agile \
     software development. A spike is used to research a technical question or evaluate a possible \
     solution before committing to full implementation. The output of a spike is typically a \
     proof of concept and a decision document (GO/NO-GO), rather than production-ready code. \
     This allows teams to make informed architectural decisions without over-investing in dead ends.",
    "The Accelerate framework on Apple platforms provides vectorized mathematical operations \
     for signal processing, linear algebra, and machine learning. vDSP (vector digital signal \
     processing) functions operate on arrays using SIMD instructions that process multiple \
     elements per CPU cycle. For embedding normalization at dimension 768, this means roughly \
     4-8x speedup over scalar loops on ARM NEON, and additional gains are available via the \
     ANE for supported CoreML operations.",
    "The BGE (BAAI General Embedding) model family from the Beijing Academy of Artificial \
     Intelligence offers a range of model sizes optimized for semantic retrieval tasks. BGE-base \
     has 109 million parameters and produces 768-dimensional embeddings. It was trained using \
     a combination of contrastive learning and knowledge distillation, achieving competitive \
     performance on the BEIR benchmark while remaining small enough for on-device deployment.",
    "Token type IDs, also called segment IDs, are used in BERT to distinguish between two \
     sentence segments in sentence-pair tasks. For single-sentence inputs, all token type IDs \
     are zero. The model uses these IDs as an additional input embedding that is summed with \
     the token embeddings and positional embeddings before the first transformer layer. \
     Single-sentence embedding tasks always pass all-zero token type IDs.",
];

/// Build the 200-sentence benchmark corpus (replicates bench.py's SENTENCES list).
///
/// Order: SHORT×4, MEDIUM×3, LONG×6, truncated to 200.
pub fn bench_sentences() -> Vec<&'static str> {
    let mut pool = Vec::with_capacity(260);
    for _ in 0..4 {
        pool.extend_from_slice(SHORT);
    }
    for _ in 0..3 {
        pool.extend_from_slice(MEDIUM);
    }
    for _ in 0..6 {
        pool.extend_from_slice(LONG);
    }
    pool.truncate(200);
    pool
}

/// The 50-sentence parity reference set (from verify-embedding-parity.py).
pub const PARITY_SENTENCES: &[&str] = &[
    "The quick brown fox jumps over the lazy dog.",
    "Artificial intelligence is transforming the way we work.",
    "Machine learning models require large amounts of training data.",
    "Apple Silicon provides significant performance improvements.",
    "The CoreML framework enables on-device machine learning.",
    "BERT-based models excel at understanding context in text.",
    "Embeddings map text into high-dimensional vector spaces.",
    "Cosine similarity measures the angle between two vectors.",
    "Knowledge graphs represent relationships between entities.",
    "Natural language processing has advanced rapidly in recent years.",
    "The Swift programming language was designed for safety and performance.",
    "Hummingbird is a lightweight HTTP server framework for Swift.",
    "CoreML models can run on the Apple Neural Engine for efficiency.",
    "Sentence embeddings capture semantic meaning of text.",
    "The bge-base-en-v1.5 model produces 768-dimensional embeddings.",
    "WordPiece tokenization is used by BERT-family models.",
    "L2 normalization ensures vectors lie on the unit hypersphere.",
    "CLS token pooling extracts the sentence-level representation.",
    "Python and Swift can interoperate through HTTP or sockets.",
    "The attention mechanism allows models to focus on relevant tokens.",
    "Transformers have become the dominant architecture in NLP.",
    "On-device inference reduces privacy risk and latency.",
    "The Apple Neural Engine is specialized for ML workloads.",
    "Graph databases store data as nodes and relationships.",
    "LadybugDB is an embedded graph database for the Liminis project.",
    "Vector search finds semantically similar passages efficiently.",
    "Jaccard similarity measures overlap between two sets.",
    "Retrieval-augmented generation improves factual accuracy.",
    "The Unix domain socket provides low-latency IPC on the same host.",
    "Benchmark results should include p50 and p95 latency statistics.",
    "Cold-start time includes process initialization and model loading.",
    "Memory footprint affects how many models can run concurrently.",
    "Float16 quantization reduces model size with minimal quality loss.",
    "The position embedding encodes the location of each token.",
    "Token type IDs distinguish sentence A from sentence B in BERT.",
    "Padding tokens are masked out in attention computations.",
    "Special tokens like [CLS] and [SEP] frame the input sequence.",
    "The hidden dimension of bge-base-en-v1.5 is 768.",
    "Normalization is critical for consistent embedding comparisons.",
    "The production embedding path uses sentence-transformers in Python.",
    "Async/await simplifies concurrent code in Swift 5.5+.",
    "Actors in Swift provide data-race-free state isolation.",
    "HTTP 503 indicates the service is temporarily unavailable.",
    "Foundation Models requires Apple Intelligence to be enabled.",
    "The conversion script uses torch.jit.trace with return_dict=False.",
    "MLModel.prediction() is documented as thread-safe by Apple.",
    "The spike's goal is a GO/NO-GO decision, not production code.",
    "Coremltools 8.1 requires numpy < 2.4 to avoid scalar conversion errors.",
    "The mlpackage format stores the compiled model and weights.",
    "Benchmark harness should report wall-time, not CPU time.",
];
