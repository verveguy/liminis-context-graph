#!/usr/bin/env python3
"""
REQ-15 / SC-001: Generate HuggingFace BertTokenizerFast reference tokenization for
100 sentences, writing tokenizer_reference.json. This file is consumed by the Swift
tokenizer parity test (Task 11) to verify byte-for-byte match with swift-transformers.

Usage:
    uv run verify-tokenizer-parity.py [--output tokenizer_reference.json]
"""
# /// script
# requires-python = ">=3.11,<3.13"
# dependencies = [
#     "transformers>=4.36.2,<5.0",
# ]
# ///

import argparse
import json

REFERENCE_SENTENCES = [
    # Basic sentences
    "Hello, world!",
    "The quick brown fox jumps over the lazy dog.",
    "Artificial intelligence is transforming the way we work.",
    "Machine learning models require large amounts of training data.",
    "Apple Silicon provides significant performance improvements.",
    # Technical content
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
    # Special characters and punctuation
    "It's a beautiful day, isn't it?",
    "She said: \"I'll be there at 3 p.m.\"",
    "The price is $19.99 — that's a bargain!",
    "Use & for 'and' in HTML entities.",
    "Numbers: 1, 2, 3, 100, 1000, 1000000.",
    # Longer sentences (stress-test truncation)
    "This is a very long sentence that contains many words and is designed to test whether the tokenizer correctly handles text that approaches or exceeds the maximum sequence length of five hundred and twelve tokens when processing input.",
    "The implementation of the CoreML embedding handler in Swift involves loading the machine learning model package from disk at startup, configuring the compute units to prefer the Apple Neural Engine, and then for each input text, running the WordPiece tokenizer to convert the text into token IDs, calling the model to obtain the last hidden state, applying CLS pooling to extract the sentence-level embedding, and finally applying L2 normalization to obtain a unit vector.",
    # Edge cases
    "",
    "a",
    "  ",
    "UPPERCASE WORDS",
    "lowercase words",
    "Mixed Case Words",
    "1234567890",
    "!@#$%^&*()",
    # Contractions and apostrophes
    "don't",
    "can't",
    "won't",
    "I'm",
    "they're",
    "it's",
    "we've",
    "I'd",
    # Technical abbreviations
    "API",
    "HTTP",
    "JSON",
    "SQL",
    "ML",
    "AI",
    "NLP",
    "BERT",
    "GPT",
    "LLM",
    # Hyphenated words
    "state-of-the-art",
    "well-known",
    "high-dimensional",
    "pre-trained",
    "fine-tuning",
    # Numbers and units
    "3.14159265",
    "100 MB",
    "768 dimensions",
    "50 ms",
    "1024 tokens",
    # URLs and code-like text (tokenized as individual subwords)
    "https://huggingface.co/BAAI/bge-base-en-v1.5",
    "import torch",
    "def embed(text: str) -> list[float]:",
    "x = np.zeros((1, 512, 768), dtype=np.float32)",
    "mlmodel.predict({'input_ids': ids})",
    # Sentences about the project
    "The Liminis project uses LadybugDB for embedded graph storage.",
    "Graphiti provides a knowledge graph service over stdio MCP.",
    "The local-inference binary serves both LLM and embedding endpoints.",
    "Foundation Models require Apple Intelligence to be enabled on the device.",
    "The benchmark harness measures cold-start, latency p50/p95, and memory.",
    "GO with caveats: the model works but requires convert-at-install for distribution.",
    # Non-ASCII characters that exercise multi-byte UTF-8 paths in the tokenizer
    "café latte",
    "résumé",
    "naïve",
    # More regular sentences for good measure
    "The attention mechanism allows models to focus on relevant tokens.",
    "Transformers have become the dominant architecture in NLP.",
    "On-device inference reduces privacy risk and latency.",
    "The Apple Neural Engine is specialized for ML workloads.",
    "Graph databases store data as nodes and relationships.",
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
    "The spike's goal is a GO/NO-GO decision, not production code.",
    "Async/await simplifies concurrent code in Swift 5.5+.",
    "Actors in Swift provide data-race-free state isolation.",
    "HTTP 503 indicates the service is temporarily unavailable.",
]

assert len(REFERENCE_SENTENCES) == 100, f"Expected 100 sentences, got {len(REFERENCE_SENTENCES)}"


def main():
    parser = argparse.ArgumentParser(
        description="Generate HuggingFace tokenizer reference JSON for Swift parity tests"
    )
    parser.add_argument("--output", default="tokenizer_reference.json",
                        help="Output JSON file path")
    parser.add_argument("--model", default="BAAI/bge-base-en-v1.5",
                        help="HuggingFace model name for tokenizer")
    parser.add_argument("--max-length", type=int, default=512,
                        help="Max sequence length (matches Swift handler)")
    args = parser.parse_args()

    from transformers import AutoTokenizer

    print(f"Loading tokenizer: {args.model}")
    tokenizer = AutoTokenizer.from_pretrained(args.model, use_fast=True)

    print(f"Tokenizing {len(REFERENCE_SENTENCES)} sentences (max_length={args.max_length})...")
    records = []
    for i, sentence in enumerate(REFERENCE_SENTENCES):
        enc = tokenizer(
            sentence,
            padding="max_length",
            max_length=args.max_length,
            truncation=True,
        )
        records.append({
            "sentence": sentence,
            "input_ids": enc["input_ids"],
            "attention_mask": enc["attention_mask"],
            "token_type_ids": enc.get("token_type_ids", [0] * args.max_length),
        })

    output = {
        "model": args.model,
        "max_length": args.max_length,
        "tokenizer_class": type(tokenizer).__name__,
        "vocab_size": tokenizer.vocab_size,
        "cls_token_id": tokenizer.cls_token_id,
        "sep_token_id": tokenizer.sep_token_id,
        "pad_token_id": tokenizer.pad_token_id,
        "unk_token_id": tokenizer.unk_token_id,
        "sentences": records,
    }

    with open(args.output, "w") as f:
        json.dump(output, f, indent=2)

    print(f"Written: {args.output}")
    print(f"  vocab_size: {tokenizer.vocab_size}")
    print(f"  cls_token_id: {tokenizer.cls_token_id}")
    print(f"  sep_token_id: {tokenizer.sep_token_id}")
    print(f"  pad_token_id: {tokenizer.pad_token_id}")
    print(f"  unk_token_id: {tokenizer.unk_token_id}")

    # Quick sanity check: verify first sentence tokenization
    first = records[0]
    cls_id = tokenizer.cls_token_id
    sep_id = tokenizer.sep_token_id
    assert first["input_ids"][0] == cls_id, f"First token should be [CLS]={cls_id}"
    print(f"\nSanity check passed: First token is [CLS]={cls_id}")
    print(f"Done. {len(records)} reference tokenizations written to {args.output}")


if __name__ == "__main__":
    main()
