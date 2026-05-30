#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "coremltools>=8.0",
#     "numpy>=1.24",
# ]
# ///
"""
Generate stub-bge-base[-fp16].mlpackage — tiny synthetic CoreML models for integration tests.

I/O Contract (must match CoreMLEmbeddingActor expectations):
  Inputs:
    input_ids        Int32 [1, 512]
    attention_mask   Int32 [1, 512]
    token_type_ids   Int32 [1, 512]
  Output:
    last_hidden_state  Float32 or Float16 [1, 512, 768]

The actual numerical output is meaningless — it just needs to satisfy the shape
contract so CoreMLEmbeddingActor can run CLS pooling and L2 normalization on it.

Usage:
  # Default: fp32 weights / fp32 output (legacy fixture, matches stub-bge-base.mlpackage).
  uv run generate-stub-model.py

  # fp16: matches the production convert-embedding-model.py setting
  # (compute_precision=FLOAT16). Used by integration tests parameterized over both
  # supported output dtypes — guards against PR #805 / issue #810 style regressions.
  uv run generate-stub-model.py --precision fp16

  Tested with coremltools 8.1 on macOS 14+.

.mlmodelc fallback:
  If swift test times out during MLModel.compileModel(at:), pre-compile the stub
  and commit the resulting .mlmodelc instead. Use this Swift one-liner:

    swift -e '
    import CoreML, Foundation
    let pkg = URL(fileURLWithPath: "stub-bge-base.mlpackage")
    let compiled = try await MLModel.compileModel(at: pkg)
    try FileManager.default.copyItem(at: compiled,
        to: URL(fileURLWithPath: "stub-bge-base.mlmodelc"))
    print("compiled →", compiled.path)
    '

  Then update the modelURL in LocalInferenceIntegrationTests.swift to point at
  stub-bge-base.mlmodelc instead of stub-bge-base.mlpackage.
"""

import argparse
import os
import shutil
import numpy as np
import coremltools as ct
from coremltools.converters.mil import Builder as mb

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))

SEQ_LEN = 512
HIDDEN = 768


def build_program(output_mil_dtype: str):
    """Build the MIL program. `output_mil_dtype` is "fp32" or "fp16" and controls
    the dtype of the final cast op (which determines the published output dtype)."""
    np.random.seed(42)
    # Scale weights down hard so matmul([1,512] · W) stays in the fp16 representable
    # range regardless of token-id magnitude (bge-base ids go up to ~30000; without
    # scaling the dot product easily overflows fp16's ~65504 ceiling and produces NaN
    # CLS vectors that fail JSON encoding). 1e-6 keeps every reasonable input finite.
    W_SCALE = 1e-6
    W = (np.random.randn(SEQ_LEN, HIDDEN) * W_SCALE).astype(np.float16)

    # opset_version=iOS17 is required so the MIL program can emit fp16 ops/outputs.
    # Without it the converter rejects the fp16 cast as unsupported for the default opset.
    @mb.program(
        input_specs=[
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
        ],
        opset_version=ct.target.iOS17,
    )
    def prog(input_ids, attention_mask, token_type_ids):
        # Cast input_ids to float32 for matmul
        ids_f = mb.cast(x=input_ids, dtype="fp32", name="ids_float")  # [1, 512]

        # Random projection: [1, 512] × [512, 768] → [1, 768]
        w_const = mb.const(val=W.astype(np.float32), name="W")
        projected = mb.matmul(x=ids_f, y=w_const, name="projected")  # [1, 768]

        # Tile [1, 768] → [1, 512, 768] so CLS pooling at [0,0,:] picks the real vector.
        expanded = mb.expand_dims(x=projected, axes=[1], name="expanded")              # [1, 1, 768]
        hidden = mb.tile(x=expanded, reps=[1, SEQ_LEN, 1], name="hidden_tiled")        # [1, 512, 768]

        # Route attention_mask and token_type_ids through a no-op so CoreML validation
        # does not complain about unused inputs. Cast to float, scale by 0, add to hidden.
        mask_f = mb.cast(x=attention_mask, dtype="fp32", name="mask_float")
        mask_zero = mb.mul(x=mask_f, y=np.float32(0.0), name="mask_zero")
        mask_exp = mb.expand_dims(x=mask_zero, axes=[2], name="mask_exp")              # [1, 512, 1]

        types_f = mb.cast(x=token_type_ids, dtype="fp32", name="types_float")
        types_zero = mb.mul(x=types_f, y=np.float32(0.0), name="types_zero")
        types_exp = mb.expand_dims(x=types_zero, axes=[2], name="types_exp")           # [1, 512, 1]

        summed = mb.add(x=hidden, y=mask_exp, name="add_mask")
        summed = mb.add(x=summed, y=types_exp, name="add_types")
        # The name of the final op becomes the output feature name in the CoreML spec.
        # CoreMLEmbeddingActor.embed() fetches the output by name "last_hidden_state".
        # An explicit cast here is what determines the published output dtype —
        # compute_precision alone does not propagate to the final feature dtype.
        out = mb.cast(x=summed, dtype=output_mil_dtype, name="last_hidden_state")

        return out

    return prog


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate a stub BGE CoreML model for tests.")
    parser.add_argument(
        "--precision",
        choices=["fp16", "fp32"],
        default="fp32",
        help="Output / compute precision. fp32 (default) matches the legacy stub; fp16 matches the production convert-embedding-model.py setting.",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="Output .mlpackage path. Defaults to stub-bge-base[-fp16].mlpackage alongside this script.",
    )
    args = parser.parse_args()

    if args.output is None:
        suffix = "" if args.precision == "fp32" else "-fp16"
        output_path = os.path.join(SCRIPT_DIR, f"stub-bge-base{suffix}.mlpackage")
    else:
        output_path = args.output

    # Getting fp16 in the published output feature requires three things together:
    #   1. opset_version=iOS17 on the @mb.program (set inside build_program)
    #   2. minimum_deployment_target=iOS17 here
    #   3. dtype=np.float16 on the ct.TensorType output
    # Without all three, coremltools silently coerces the output back to fp32.
    # iOS17 maps to macOS14, matching production convert-embedding-model.py.
    output_np_dtype = np.float16 if args.precision == "fp16" else np.float32
    convert_kwargs = dict(
        convert_to="mlprogram",
        minimum_deployment_target=ct.target.iOS17,
        inputs=[
            ct.TensorType(name="input_ids",      shape=(1, SEQ_LEN), dtype=np.int32),
            ct.TensorType(name="attention_mask", shape=(1, SEQ_LEN), dtype=np.int32),
            ct.TensorType(name="token_type_ids", shape=(1, SEQ_LEN), dtype=np.int32),
        ],
        outputs=[
            ct.TensorType(name="last_hidden_state", dtype=output_np_dtype),
        ],
    )
    if args.precision == "fp16":
        convert_kwargs["compute_precision"] = ct.precision.FLOAT16
    else:
        convert_kwargs["compute_precision"] = ct.precision.FLOAT32

    output_mil_dtype = "fp16" if args.precision == "fp16" else "fp32"
    model = ct.convert(build_program(output_mil_dtype), **convert_kwargs)

    shutil.rmtree(output_path, ignore_errors=True)
    model.save(output_path)
    print(f"Saved: {output_path}")

    total = sum(
        os.path.getsize(os.path.join(dp, f))
        for dp, dn, fn in os.walk(output_path)
        for f in fn
    )
    print(f"Size:  {total / 1024 / 1024:.2f} MB")


if __name__ == "__main__":
    main()
