#!/usr/bin/env python3
"""
Convert a HuggingFace BGE embedding model to Core ML (.mlpackage) for on-device embeddings.

Defaults to BAAI/bge-base-en-v1.5 (768-dim, ~85-90 MB float16).
Also works with BAAI/bge-small-en-v1.5 (384-dim) via --model.

Uses torch.jit.trace with a wrapper that forces return_dict=False (plain tuple output),
which avoids the ModelOutput dataclass tracing issues in transformers. Fixed input shapes
(batch=1, seq_len=512) match the Swift handler, which pads/truncates to exactly this size.

Usage:
    uv run convert-embedding-model.py [--model BAAI/bge-base-en-v1.5] [--output bge-base-en-v1.5.mlpackage]
                                      [--revision <commit-hash>]
"""
# /// script
# requires-python = ">=3.11,<3.13"
# dependencies = [
#     "torch==2.4.0",
#     "transformers>=4.36.2,<5.0",
#     "coremltools==8.1",
#     "numpy<2.4",
# ]
# ///

import argparse
import os
import shutil

# Pinned HuggingFace commit for BAAI/bge-base-en-v1.5.
# Update this constant when the upstream model changes (run refresh-test-fixtures.sh afterwards).
PINNED_BGE_REVISION = "a5beb1e3e68b9ab74eb54cfd186867f64f240e1a"


def convert(model_name: str, output_path: str, revision: str | None = None, max_seq_length: int = 512):
    import torch
    import torch.nn as nn
    import numpy as np
    import coremltools as ct
    from transformers import AutoModel

    rev_display = revision[:8] + "..." if revision else "latest"
    print(f"Step 1: Load {model_name} from HuggingFace (revision={rev_display})...")
    base_model = AutoModel.from_pretrained(model_name, revision=revision)
    base_model.eval()

    # Wrapper that returns plain tensor (last_hidden_state) instead of ModelOutput.
    # ModelOutput dataclasses cause tracing failures with torch.jit.trace because
    # they involve dynamic dict-like construction not supported by the tracer.
    class TraceableWrapper(nn.Module):
        def __init__(self, model):
            super().__init__()
            self.model = model

        def forward(self, input_ids, attention_mask, token_type_ids):
            out = self.model(
                input_ids=input_ids,
                attention_mask=attention_mask,
                token_type_ids=token_type_ids,
                return_dict=False,
            )
            # out[0] is always last_hidden_state for feature-extraction
            return out[0]

    model = TraceableWrapper(base_model)
    model.eval()

    print(f"Step 2: Trace with torch.jit.trace (seq_len={max_seq_length})...")
    # Fixed shape: batch=1, seq_len=max_seq_length (Swift handler always pads to this).
    # BERT forward pass with return_dict=False has no Python-level data-dependent control
    # flow, so torch.jit.trace produces correct results for all inputs of this shape.
    dummy_input_ids = torch.ones((1, max_seq_length), dtype=torch.long)
    dummy_attention_mask = torch.ones((1, max_seq_length), dtype=torch.long)
    dummy_token_type_ids = torch.zeros((1, max_seq_length), dtype=torch.long)

    with torch.no_grad():
        traced = torch.jit.trace(
            model,
            (dummy_input_ids, dummy_attention_mask, dummy_token_type_ids),
            strict=False,
        )

    print("Step 3: Convert traced model → Core ML (float16)...")
    cml = ct.convert(
        traced,
        inputs=[
            ct.TensorType(name="input_ids",      shape=(1, max_seq_length), dtype=np.int32),
            ct.TensorType(name="attention_mask",  shape=(1, max_seq_length), dtype=np.int32),
            ct.TensorType(name="token_type_ids", shape=(1, max_seq_length), dtype=np.int32),
        ],
        outputs=[ct.TensorType(name="last_hidden_state")],
        compute_precision=ct.precision.FLOAT16,
        compute_units=ct.ComputeUnit.ALL,
        minimum_deployment_target=ct.target.macOS14,
    )

    print(f"Step 4: Save to {output_path}...")
    if os.path.exists(output_path):
        shutil.rmtree(output_path)
    cml.save(output_path)

    size_mb = sum(
        os.path.getsize(os.path.join(dp, f))
        for dp, _, filenames in os.walk(output_path)
        for f in filenames
    ) / (1024 * 1024)
    print(f"  Done! Model saved: {output_path} ({size_mb:.1f} MB)")

    print("Step 5: Verify by loading and running...")
    loaded = ct.models.MLModel(output_path)
    spec = loaded.get_spec()
    inputs = [inp.name for inp in spec.description.input]
    outputs = [out.name for out in spec.description.output]
    print(f"  Inputs:  {inputs}")
    print(f"  Outputs: {outputs}")

    # Sanity-check: one forward pass
    test_ids = np.ones((1, max_seq_length), dtype=np.int32)
    test_mask = np.ones((1, max_seq_length), dtype=np.int32)
    test_types = np.zeros((1, max_seq_length), dtype=np.int32)
    out = loaded.predict({"input_ids": test_ids, "attention_mask": test_mask, "token_type_ids": test_types})
    output_key = list(out.keys())[0]
    output_shape = out[output_key].shape
    print(f"  Output key: '{output_key}', shape: {output_shape}")
    print("  Sanity check passed.")


def main():
    parser = argparse.ArgumentParser(description="Convert a HuggingFace embedding model to Core ML")
    parser.add_argument("--model", default="BAAI/bge-base-en-v1.5", help="HuggingFace model name")
    parser.add_argument("--output", default=None, help="Output .mlpackage path")
    parser.add_argument(
        "--revision",
        default=PINNED_BGE_REVISION,
        help="HuggingFace commit hash to pin (defaults to PINNED_BGE_REVISION constant)",
    )
    parser.add_argument("--max-seq-length", type=int, default=512, help="Max sequence length")
    args = parser.parse_args()

    if args.output is None:
        args.output = args.model.split("/")[-1] + ".mlpackage"

    convert(args.model, args.output, args.revision, args.max_seq_length)


if __name__ == "__main__":
    main()
