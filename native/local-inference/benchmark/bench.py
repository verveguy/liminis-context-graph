#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11,<3.13"
# dependencies = [
#     "sentence-transformers>=2.7.0,<3.0",
#     "torch==2.4.0",
#     "numpy<2.4",
# ]
# ///
"""
Spike benchmark: CoreML BGE-base-en-v1.5 (via Swift sidecar) vs Python sentence-transformers.

Usage:
    # 1. Build and start the sidecar first:
    #    swift build -c release 2>&1
    #    LOCAL_INFERENCE_EMBEDDING_MODEL=../bge-base-en-v1.5.mlpackage \
    #      .build/release/local-inference &

    # 2. Run benchmark:
    uv run benchmark/bench.py [--socket PATH] [--sentences N] [--warmup N]

Output: results/results-<timestamp>.json + results/results-<timestamp>.md
"""

from __future__ import annotations

import argparse
import json
import socket
import statistics
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# ---------------------------------------------------------------------------
# Sentence corpus (200 sentences: short, medium, long)
# ---------------------------------------------------------------------------

SHORT = [
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
]

MEDIUM = [
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
]

LONG = [
    (
        "Knowledge graphs are structured representations of information that capture entities and their "
        "relationships in a graph format. They have been used extensively in search engines, question "
        "answering systems, and recommendation engines. Recent work has explored combining knowledge "
        "graphs with large language models to improve factual accuracy and enable multi-hop reasoning."
    ),
    (
        "The transformer architecture, introduced in the paper 'Attention Is All You Need' by Vaswani "
        "et al. in 2017, replaced recurrent neural networks with self-attention mechanisms. This allowed "
        "for much more efficient parallel training and better capture of long-range dependencies in text. "
        "BERT, GPT, and their successors are all built on this foundational architecture."
    ),
    (
        "Apple Silicon chips, including the M1, M2, M3, and M4 families, integrate CPU, GPU, Neural "
        "Engine, and memory on a single die. This unified memory architecture eliminates the need to "
        "copy data between separate memory pools, reducing latency for machine learning inference. The "
        "Neural Engine supports INT8 and FP16 operations optimized for transformer attention patterns."
    ),
    (
        "CoreML Tools is a Python library for converting models from frameworks like PyTorch, "
        "TensorFlow, and scikit-learn into the CoreML format. The conversion process involves tracing "
        "or scripting the model to capture its computational graph, then converting operations to "
        "CoreML's intermediate representation. The resulting .mlpackage can be integrated directly "
        "into macOS, iOS, and other Apple platform applications."
    ),
    (
        "Semantic search systems typically involve encoding queries and documents into a shared "
        "embedding space, then retrieving the nearest neighbors by cosine similarity or inner product. "
        "The quality of these embeddings determines recall and precision. Models like BGE (BAAI General "
        "Embedding) are specifically optimized for retrieval tasks using contrastive learning objectives "
        "that pull similar pairs together and push dissimilar pairs apart in embedding space."
    ),
    (
        "The Swift programming language was designed by Apple to be safe, fast, and expressive. Its "
        "type system prevents common programming errors like null pointer dereferences, and its "
        "performance characteristics make it suitable for systems programming. Swift 6 introduced "
        "strict concurrency checking, making data races compile-time errors rather than runtime bugs. "
        "This has significant implications for server-side Swift development and actor-based APIs."
    ),
    (
        "Graph neural networks (GNNs) extend deep learning to graph-structured data by performing "
        "iterative message passing between nodes and their neighbors. Applications include drug "
        "discovery, where molecular graphs represent chemical compounds, social network analysis, "
        "and knowledge graph completion. GNNs can also be combined with transformer architectures "
        "in hybrid models that leverage both local graph structure and global attention."
    ),
    (
        "The Unix domain socket is an inter-process communication mechanism that allows processes "
        "on the same host to communicate through the filesystem namespace. Unlike TCP sockets, "
        "Unix domain sockets skip the network stack entirely, providing lower latency and higher "
        "throughput for local IPC. They are commonly used for database connections, service meshes, "
        "and other local service communication patterns where network overhead is undesirable."
    ),
    (
        "Retrieval-augmented generation (RAG) combines information retrieval with language model "
        "generation. A retriever fetches relevant context from a corpus based on the query, and a "
        "generator conditions its output on both the query and the retrieved context. This approach "
        "allows language models to access up-to-date information without expensive fine-tuning, and "
        "provides a mechanism for attribution since the retrieved sources can be shown to users."
    ),
    (
        "L2 normalization maps vectors to the unit hypersphere, making cosine similarity equivalent "
        "to dot product similarity. This simplification is important for efficient approximate nearest "
        "neighbor search using algorithms like HNSW or IVF, which are optimized for inner product "
        "computations. Apple's Accelerate framework provides vectorized BLAS and vDSP routines that "
        "can compute L2 norms and dot products orders of magnitude faster than naive Python loops."
    ),
    (
        "The Graphiti framework provides temporal knowledge graph capabilities, tracking when facts "
        "were added and modified. This temporal dimension is crucial for applications that need to "
        "reason about changing states of the world, such as tracking the evolution of a codebase, "
        "monitoring project status, or understanding how relationships between people and organizations "
        "change over time. Graphiti integrates with FalkorDB for persistent graph storage and retrieval."
    ),
    (
        "Spike-based development, or time-boxed technical exploration, is a common practice in agile "
        "software development. A spike is used to research a technical question or evaluate a possible "
        "solution before committing to full implementation. The output of a spike is typically a "
        "proof of concept and a decision document (GO/NO-GO), rather than production-ready code. "
        "This allows teams to make informed architectural decisions without over-investing in dead ends."
    ),
    (
        "The Accelerate framework on Apple platforms provides vectorized mathematical operations "
        "for signal processing, linear algebra, and machine learning. vDSP (vector digital signal "
        "processing) functions operate on arrays using SIMD instructions that process multiple "
        "elements per CPU cycle. For embedding normalization at dimension 768, this means roughly "
        "4-8x speedup over scalar loops on ARM NEON, and additional gains are available via the "
        "ANE for supported CoreML operations."
    ),
    (
        "The BGE (BAAI General Embedding) model family from the Beijing Academy of Artificial "
        "Intelligence offers a range of model sizes optimized for semantic retrieval tasks. BGE-base "
        "has 109 million parameters and produces 768-dimensional embeddings. It was trained using "
        "a combination of contrastive learning and knowledge distillation, achieving competitive "
        "performance on the BEIR benchmark while remaining small enough for on-device deployment."
    ),
    (
        "Token type IDs, also called segment IDs, are used in BERT to distinguish between two "
        "sentence segments in sentence-pair tasks. For single-sentence inputs, all token type IDs "
        "are zero. The model uses these IDs as an additional input embedding that is summed with "
        "the token embeddings and positional embeddings before the first transformer layer. "
        "Single-sentence embedding tasks always pass all-zero token type IDs."
    ),
]

SENTENCES: list[str] = []
# Fill to 200 by cycling through categories
import itertools

_pool = list(itertools.chain(SHORT * 4, MEDIUM * 3, LONG * 6))  # ~200
SENTENCES = _pool[:200]
assert len(SENTENCES) == 200, f"Expected 200 sentences, got {len(SENTENCES)}"


# ---------------------------------------------------------------------------
# Unix socket HTTP helper
# ---------------------------------------------------------------------------

class UnixSocketHTTP:
    """Minimal HTTP/1.1 client over a Unix domain socket."""

    def __init__(self, socket_path: str) -> None:
        self.socket_path = socket_path

    def post(self, path: str, body: dict[str, Any]) -> dict[str, Any]:
        payload = json.dumps(body).encode()
        request = (
            f"POST {path} HTTP/1.1\r\n"
            f"Host: localhost\r\n"
            f"Content-Type: application/json\r\n"
            f"Content-Length: {len(payload)}\r\n"
            f"Connection: close\r\n"
            f"\r\n"
        ).encode() + payload

        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(self.socket_path)
            sock.sendall(request)
            raw = b""
            while chunk := sock.recv(65536):
                raw += chunk
        finally:
            sock.close()

        # Parse HTTP response
        header_end = raw.find(b"\r\n\r\n")
        body_bytes = raw[header_end + 4:]
        response = json.loads(body_bytes.decode())
        return response


# ---------------------------------------------------------------------------
# CoreML benchmark (via sidecar HTTP)
# ---------------------------------------------------------------------------

def benchmark_coreml(client: UnixSocketHTTP, sentences: list[str], warmup: int) -> dict[str, Any]:
    print(f"  Warming up CoreML ({warmup} calls)...")
    for i in range(warmup):
        client.post("/v1/embeddings", {"input": sentences[i % len(sentences)], "model": "coreml-bge"})

    print(f"  Benchmarking CoreML ({len(sentences)} individual calls)...")
    latencies_ms: list[float] = []
    first_embedding: list[float] | None = None

    for sentence in sentences:
        t0 = time.perf_counter()
        resp = client.post("/v1/embeddings", {"input": sentence, "model": "coreml-bge"})
        elapsed_ms = (time.perf_counter() - t0) * 1000
        latencies_ms.append(elapsed_ms)
        if first_embedding is None:
            first_embedding = resp["data"][0]["embedding"]

    # Batch test: all 200 sentences in one call
    print(f"  Benchmarking CoreML batch ({len(sentences)} sentences in one call)...")
    batch_times: list[float] = []
    for _ in range(3):
        t0 = time.perf_counter()
        client.post("/v1/embeddings", {"input": sentences, "model": "coreml-bge"})
        batch_times.append((time.perf_counter() - t0) * 1000)

    latencies_ms.sort()
    return {
        "p50_ms": statistics.median(latencies_ms),
        "p95_ms": latencies_ms[int(len(latencies_ms) * 0.95)],
        "p99_ms": latencies_ms[int(len(latencies_ms) * 0.99)],
        "mean_ms": statistics.mean(latencies_ms),
        "min_ms": min(latencies_ms),
        "max_ms": max(latencies_ms),
        "throughput_per_sec": 1000 / statistics.median(latencies_ms),
        "batch_mean_ms": statistics.mean(batch_times),
        "batch_throughput_per_sec": len(sentences) * 1000 / statistics.mean(batch_times),
        "first_embedding_dim": len(first_embedding) if first_embedding else None,
    }


# ---------------------------------------------------------------------------
# Python sentence-transformers benchmark
# ---------------------------------------------------------------------------

def benchmark_python(sentences: list[str], warmup: int) -> dict[str, Any]:
    from sentence_transformers import SentenceTransformer  # type: ignore

    print("  Loading sentence-transformers BGE-base-en-v1.5...")
    model = SentenceTransformer("BAAI/bge-base-en-v1.5")

    print(f"  Warming up Python ({warmup} calls)...")
    for i in range(warmup):
        model.encode([sentences[i % len(sentences)]])

    print(f"  Benchmarking Python ({len(sentences)} individual calls)...")
    latencies_ms: list[float] = []
    first_embedding: list[float] | None = None

    for sentence in sentences:
        t0 = time.perf_counter()
        result = model.encode([sentence])
        elapsed_ms = (time.perf_counter() - t0) * 1000
        latencies_ms.append(elapsed_ms)
        if first_embedding is None:
            first_embedding = result[0].tolist()

    # Batch test
    print(f"  Benchmarking Python batch ({len(sentences)} sentences in one call)...")
    batch_times: list[float] = []
    for _ in range(3):
        t0 = time.perf_counter()
        model.encode(sentences, batch_size=32, show_progress_bar=False)
        batch_times.append((time.perf_counter() - t0) * 1000)

    latencies_ms.sort()
    return {
        "p50_ms": statistics.median(latencies_ms),
        "p95_ms": latencies_ms[int(len(latencies_ms) * 0.95)],
        "p99_ms": latencies_ms[int(len(latencies_ms) * 0.99)],
        "mean_ms": statistics.mean(latencies_ms),
        "min_ms": min(latencies_ms),
        "max_ms": max(latencies_ms),
        "throughput_per_sec": 1000 / statistics.median(latencies_ms),
        "batch_mean_ms": statistics.mean(batch_times),
        "batch_throughput_per_sec": len(sentences) * 1000 / statistics.mean(batch_times),
        "first_embedding_dim": len(first_embedding) if first_embedding else None,
    }


# ---------------------------------------------------------------------------
# Report generation
# ---------------------------------------------------------------------------

def write_results(results: dict[str, Any], out_dir: Path, timestamp: str) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)

    json_path = out_dir / f"results-{timestamp}.json"
    json_path.write_text(json.dumps(results, indent=2))
    print(f"\nResults JSON: {json_path}")

    coreml = results["coreml"]
    python = results["python"]

    speedup_p50 = python["p50_ms"] / coreml["p50_ms"] if coreml["p50_ms"] > 0 else float("inf")
    speedup_batch = python["batch_throughput_per_sec"] / coreml["batch_throughput_per_sec"] if coreml["batch_throughput_per_sec"] > 0 else float("inf")

    md = f"""# CoreML BGE-base Benchmark Results
_Generated: {results['timestamp']}_
_Host: {results['host']}_
_Sentences: {results['n_sentences']}_

## Single-Sentence Latency

| Metric | CoreML (Swift) | Python (sentence-transformers) | Ratio |
|--------|---------------|-------------------------------|-------|
| p50 | {coreml['p50_ms']:.1f} ms | {python['p50_ms']:.1f} ms | {speedup_p50:.2f}x |
| p95 | {coreml['p95_ms']:.1f} ms | {python['p95_ms']:.1f} ms | — |
| p99 | {coreml['p99_ms']:.1f} ms | {python['p99_ms']:.1f} ms | — |
| mean | {coreml['mean_ms']:.1f} ms | {python['mean_ms']:.1f} ms | — |
| min | {coreml['min_ms']:.1f} ms | {python['min_ms']:.1f} ms | — |
| max | {coreml['max_ms']:.1f} ms | {python['max_ms']:.1f} ms | — |
| throughput | {coreml['throughput_per_sec']:.1f} req/s | {python['throughput_per_sec']:.1f} req/s | — |

## Batch Throughput (200 sentences, 3 trials)

| Metric | CoreML (Swift) | Python (sentence-transformers) | Ratio |
|--------|---------------|-------------------------------|-------|
| batch total | {coreml['batch_mean_ms']:.0f} ms | {python['batch_mean_ms']:.0f} ms | — |
| throughput | {coreml['batch_throughput_per_sec']:.1f} sent/s | {python['batch_throughput_per_sec']:.1f} sent/s | {speedup_batch:.2f}x |

## Notes

- CoreML latency includes HTTP round-trip over Unix domain socket
- Python latency is direct function call (no IPC overhead)
- CoreML batch processes {results['n_sentences']} sentences as N sequential model predictions (fixed batch=1)
- Python batch uses sentence-transformers batch_size=32 with vectorized GPU/MPS inference
- Embedding dimension: CoreML={coreml['first_embedding_dim']}, Python={python['first_embedding_dim']}
"""
    md_path = out_dir / f"results-{timestamp}.md"
    md_path.write_text(md)
    print(f"Results Markdown: {md_path}")
    print()
    print(md)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    import platform

    parser = argparse.ArgumentParser(description="CoreML vs Python embedding benchmark")
    parser.add_argument("--socket", default="/tmp/liminis-inference.sock",
                        help="Unix socket path for the Swift sidecar")
    parser.add_argument("--sentences", type=int, default=200,
                        help="Number of sentences to benchmark")
    parser.add_argument("--warmup", type=int, default=5,
                        help="Number of warmup calls before timing")
    parser.add_argument("--skip-python", action="store_true",
                        help="Skip the Python sentence-transformers benchmark")
    parser.add_argument("--skip-coreml", action="store_true",
                        help="Skip the CoreML sidecar benchmark")
    args = parser.parse_args()

    sentences = SENTENCES[: args.sentences]
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out_dir = Path(__file__).parent / "results"

    results: dict[str, Any] = {
        "timestamp": timestamp,
        "host": platform.node(),
        "n_sentences": len(sentences),
        "coreml": {},
        "python": {},
    }

    if not args.skip_coreml:
        print(f"\n=== CoreML benchmark (socket: {args.socket}) ===")
        try:
            client = UnixSocketHTTP(args.socket)
            # Verify connection
            client.post("/v1/embeddings", {"input": "test", "model": "coreml-bge"})
            results["coreml"] = benchmark_coreml(client, sentences, args.warmup)
        except Exception as e:
            print(f"  ERROR: {e}")
            print("  Is the sidecar running? Build with: swift build -c release")
            print("  Start with: LOCAL_INFERENCE_EMBEDDING_MODEL=../bge-base-en-v1.5.mlpackage")
            print("              .build/release/local-inference")
            results["coreml"] = {"error": str(e)}

    if not args.skip_python:
        print("\n=== Python benchmark (sentence-transformers) ===")
        try:
            results["python"] = benchmark_python(sentences, args.warmup)
        except Exception as e:
            print(f"  ERROR: {e}")
            results["python"] = {"error": str(e)}

    write_results(results, out_dir, timestamp)


if __name__ == "__main__":
    main()
