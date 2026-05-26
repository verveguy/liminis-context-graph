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
Generate reference_embeddings.json for the ort/candle parity check.

Embeds PARITY_SENTENCES (50 sentences) using sentence-transformers
BAAI/bge-base-en-v1.5 with normalize_embeddings=True, then writes
the result to spikes/native-embedder/reference_embeddings.json.

Run from the spike root directory:
    uv run scripts/gen_reference.py
    # or
    pip install sentence-transformers torch numpy
    python scripts/gen_reference.py
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

# Must match common/src/corpus.rs PARITY_SENTENCES exactly
PARITY_SENTENCES = [
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
]

assert len(PARITY_SENTENCES) == 50, f"Expected 50, got {len(PARITY_SENTENCES)}"


def main() -> None:
    try:
        from sentence_transformers import SentenceTransformer
    except ImportError:
        print("ERROR: sentence-transformers not installed.", file=sys.stderr)
        print("Install: pip install sentence-transformers torch numpy", file=sys.stderr)
        sys.exit(1)

    print("Loading BAAI/bge-base-en-v1.5 ...")
    model = SentenceTransformer("BAAI/bge-base-en-v1.5")

    print(f"Embedding {len(PARITY_SENTENCES)} sentences ...")
    embeddings = model.encode(PARITY_SENTENCES, normalize_embeddings=True, show_progress_bar=True)

    output = {
        "model": "BAAI/bge-base-en-v1.5",
        "normalize_embeddings": True,
        "sentences": PARITY_SENTENCES,
        "embeddings": embeddings.tolist(),
    }

    out_path = Path(__file__).parent.parent / "reference_embeddings.json"
    out_path.write_text(json.dumps(output, indent=None))
    print(f"Written: {out_path}")
    print(f"  {len(output['embeddings'])} embeddings × {len(output['embeddings'][0])} dims")
    print(f"  File size: {out_path.stat().st_size / 1024:.0f} KB")


if __name__ == "__main__":
    main()
