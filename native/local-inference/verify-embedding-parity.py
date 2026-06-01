#!/usr/bin/env python3
"""
REQ-04 / SC-001: Verify cosine similarity between CoreML bge-base-en-v1.5 embeddings
and PyTorch sentence-transformers BGE-base embeddings on 50 reference sentences.

Passes when all 50 pairs have cosine similarity >= 0.999.

Usage:
    uv run verify-embedding-parity.py [--model-path bge-base-en-v1.5.mlpackage]
"""
# /// script
# requires-python = ">=3.11,<3.13"
# dependencies = [
#     "torch==2.4.0",
#     "transformers>=4.36.2,<5.0",
#     "sentence-transformers>=2.2",
#     "coremltools==8.1",
#     "numpy<2.4",
# ]
# ///

import argparse
import os
import sys
import numpy as np

REFERENCE_SENTENCES = [
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

assert len(REFERENCE_SENTENCES) == 50, f"Expected 50 sentences, got {len(REFERENCE_SENTENCES)}"


def get_coreml_embeddings(model_path: str, sentences: list[str], seq_len: int = 512) -> np.ndarray:
    """Run sentences through CoreML model with CLS pooling + L2 normalization."""
    import coremltools as ct
    from transformers import AutoTokenizer

    tokenizer = AutoTokenizer.from_pretrained("BAAI/bge-base-en-v1.5")
    model = ct.models.MLModel(model_path)

    embeddings = []
    for sentence in sentences:
        enc = tokenizer(
            sentence,
            padding="max_length",
            max_length=seq_len,
            truncation=True,
            return_tensors="np",
        )
        input_ids = enc["input_ids"].astype(np.int32)
        attention_mask = enc["attention_mask"].astype(np.int32)
        token_type_ids = enc.get("token_type_ids", np.zeros_like(input_ids)).astype(np.int32)

        out = model.predict({
            "input_ids": input_ids,
            "attention_mask": attention_mask,
            "token_type_ids": token_type_ids,
        })

        # CLS pooling: take the first token's hidden state
        last_hidden = out["last_hidden_state"]  # (1, seq_len, 768)
        cls_vec = last_hidden[0, 0, :]  # (768,)

        # L2 normalization
        norm = np.linalg.norm(cls_vec)
        if norm > 0:
            cls_vec = cls_vec / norm

        embeddings.append(cls_vec.astype(np.float32))

    return np.array(embeddings)


def get_pytorch_embeddings(sentences: list[str]) -> np.ndarray:
    """Run sentences through PyTorch sentence-transformers with normalization."""
    from sentence_transformers import SentenceTransformer

    model = SentenceTransformer("BAAI/bge-base-en-v1.5")
    embeddings = model.encode(sentences, normalize_embeddings=True)
    return embeddings.astype(np.float32)


def cosine_similarity(a: np.ndarray, b: np.ndarray) -> float:
    """Cosine similarity between two normalized vectors."""
    return float(np.dot(a, b) / (np.linalg.norm(a) * np.linalg.norm(b) + 1e-10))


def main():
    parser = argparse.ArgumentParser(description="Verify embedding parity: CoreML vs PyTorch BGE-base")
    parser.add_argument("--model-path", default="bge-base-en-v1.5.mlpackage",
                        help="Path to the CoreML .mlpackage")
    args = parser.parse_args()

    if not os.path.exists(args.model_path):
        print(f"ERROR: Model not found at {args.model_path}", file=sys.stderr)
        print("Run: uv run convert-embedding-model.py first", file=sys.stderr)
        sys.exit(1)

    print(f"Loading CoreML model: {args.model_path}")
    print("Loading PyTorch sentence-transformers model: BAAI/bge-base-en-v1.5")
    print(f"\nRunning {len(REFERENCE_SENTENCES)} reference sentences through both models...")

    coreml_embs = get_coreml_embeddings(args.model_path, REFERENCE_SENTENCES)
    pytorch_embs = get_pytorch_embeddings(REFERENCE_SENTENCES)

    print(f"\n{'Idx':>3}  {'Cosine Sim':>12}  {'PASS?':>6}  Sentence (first 60 chars)")
    print("-" * 90)

    threshold = 0.999
    results = []
    for i, (ce, pe, sent) in enumerate(zip(coreml_embs, pytorch_embs, REFERENCE_SENTENCES)):
        sim = cosine_similarity(ce, pe)
        passed = sim >= threshold
        results.append(sim)
        status = "PASS" if passed else "FAIL"
        print(f"{i:>3}  {sim:>12.6f}  {status:>6}  {sent[:60]}")

    sims = np.array(results)
    print(f"\n--- Summary ---")
    print(f"Min cosine similarity : {sims.min():.6f}")
    print(f"Max cosine similarity : {sims.max():.6f}")
    print(f"Mean cosine similarity: {sims.mean():.6f}")
    print(f"Passed (>= {threshold}): {(sims >= threshold).sum()} / {len(sims)}")

    if (sims < threshold).any():
        failed = [(i, s) for i, s in enumerate(results) if s < threshold]
        print(f"\nFAILED sentences:")
        for i, s in failed:
            print(f"  [{i}] sim={s:.6f}  {REFERENCE_SENTENCES[i]}")
        print(f"\nSC-001 FAILED: {len(failed)} sentence(s) below {threshold}")
        sys.exit(1)
    else:
        print(f"\nSC-001 PASSED: All {len(REFERENCE_SENTENCES)} sentences >= {threshold}")


if __name__ == "__main__":
    main()
