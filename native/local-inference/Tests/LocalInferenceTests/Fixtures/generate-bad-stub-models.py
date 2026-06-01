#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "coremltools>=8.0",
#     "numpy>=1.24",
# ]
# ///
"""
Generate three negative-path stub .mlpackage fixtures exercising the schema
guards in CoreMLEmbeddingActor.validateOutputSchema:

  stub-bge-base-bad-dtype.mlpackage       — output dtype is int32 (not in supported set; see build_bad_dtype for why fp64 won't round-trip)
  stub-bge-base-bad-shape.mlpackage       — output shape is [1, 512, 512] (wrong dimension)
  stub-bge-base-bad-output-name.mlpackage — output name is "pooler_output" (not "last_hidden_state")

All three accept the same int32 (1, 512) input schema as the production model.
Weights are tiny / arbitrary — the goal is to load successfully into MLModel
and trip the validation check, not to produce meaningful embeddings.

Usage:
  uv run generate-bad-stub-models.py
"""

import os
import shutil
import numpy as np
import coremltools as ct
from coremltools.converters.mil import Builder as mb

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))

SEQ_LEN = 512
HIDDEN = 768
WRONG_HIDDEN = 512  # used by the bad-shape fixture


def make_inputs():
    return [
        ct.TensorType(name="input_ids",      shape=(1, SEQ_LEN), dtype=np.int32),
        ct.TensorType(name="attention_mask", shape=(1, SEQ_LEN), dtype=np.int32),
        ct.TensorType(name="token_type_ids", shape=(1, SEQ_LEN), dtype=np.int32),
    ]


def absorb_unused(input_a, input_b):
    """Cast two unused int inputs to 0-valued fp32 spread over the seq dim so CoreML
    does not error on unreferenced inputs. Returns two [1, SEQ_LEN, 1] zero tensors."""
    a_f = mb.cast(x=input_a, dtype="fp32")
    a_z = mb.mul(x=a_f, y=np.float32(0.0))
    a_e = mb.expand_dims(x=a_z, axes=[2])

    b_f = mb.cast(x=input_b, dtype="fp32")
    b_z = mb.mul(x=b_f, y=np.float32(0.0))
    b_e = mb.expand_dims(x=b_z, axes=[2])
    return a_e, b_e


def save(model, output_path: str) -> None:
    shutil.rmtree(output_path, ignore_errors=True)
    model.save(output_path)
    print(f"Saved: {output_path}")


def build_bad_dtype() -> ct.models.MLModel:
    """Output last_hidden_state as int32 — not in supportedOutputDataTypes.

    coremltools' mlprogram backend silently downcasts unsupported float output
    dtypes (e.g. fp64), but it does accept int32 outputs. Casting hidden to int32
    gives us a real outside-the-set dtype that round-trips into the .mlpackage
    spec and trips embeddingOutputDtypeUnsupported in the actor.
    """
    @mb.program(
        input_specs=[
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
        ],
        opset_version=ct.target.iOS17,
    )
    def prog(input_ids, attention_mask, token_type_ids):
        ids_e = mb.expand_dims(x=input_ids, axes=[2])
        hidden = mb.tile(x=ids_e, reps=[1, 1, HIDDEN])   # [1, 512, 768] int32

        # Absorb unused inputs in int32 so we don't introduce a float branch
        # that the converter might promote the output to float.
        mask_e = mb.expand_dims(x=attention_mask, axes=[2])
        mask_z = mb.mul(x=mask_e, y=np.int32(0))
        types_e = mb.expand_dims(x=token_type_ids, axes=[2])
        types_z = mb.mul(x=types_e, y=np.int32(0))

        s = mb.add(x=hidden, y=mask_z)
        s = mb.add(x=s, y=types_z)
        return mb.cast(x=s, dtype="int32", name="last_hidden_state")

    return ct.convert(
        prog,
        convert_to="mlprogram",
        minimum_deployment_target=ct.target.iOS17,
        inputs=make_inputs(),
        outputs=[ct.TensorType(name="last_hidden_state", dtype=np.int32)],
    )


def build_bad_shape() -> ct.models.MLModel:
    """Output shape is [1, 512, 512] — wrong hidden dimension."""
    @mb.program(
        input_specs=[
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
        ],
        opset_version=ct.target.iOS17,
    )
    def prog(input_ids, attention_mask, token_type_ids):
        ids_f = mb.cast(x=input_ids, dtype="fp32")
        ids_e = mb.expand_dims(x=ids_f, axes=[2])
        # Tile to the *wrong* hidden width to trip embeddingOutputShapeMismatch.
        hidden = mb.tile(x=ids_e, reps=[1, 1, WRONG_HIDDEN])  # [1, 512, 512]

        mask_e, types_e = absorb_unused(attention_mask, token_type_ids)
        s = mb.add(x=hidden, y=mask_e)
        s = mb.add(x=s, y=types_e)
        return mb.cast(x=s, dtype="fp32", name="last_hidden_state")

    return ct.convert(
        prog,
        convert_to="mlprogram",
        minimum_deployment_target=ct.target.iOS17,
        inputs=make_inputs(),
        outputs=[ct.TensorType(name="last_hidden_state", dtype=np.float32)],
    )


def build_bad_output_name() -> ct.models.MLModel:
    """Output feature is called pooler_output, not last_hidden_state."""
    @mb.program(
        input_specs=[
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
            mb.TensorSpec(shape=(1, SEQ_LEN), dtype=ct.converters.mil.mil.types.int32),
        ],
        opset_version=ct.target.iOS17,
    )
    def prog(input_ids, attention_mask, token_type_ids):
        ids_f = mb.cast(x=input_ids, dtype="fp32")
        ids_e = mb.expand_dims(x=ids_f, axes=[2])
        hidden = mb.tile(x=ids_e, reps=[1, 1, HIDDEN])

        mask_e, types_e = absorb_unused(attention_mask, token_type_ids)
        s = mb.add(x=hidden, y=mask_e)
        s = mb.add(x=s, y=types_e)
        # Final op is named `pooler_output` → that becomes the output feature name.
        return mb.cast(x=s, dtype="fp32", name="pooler_output")

    return ct.convert(
        prog,
        convert_to="mlprogram",
        minimum_deployment_target=ct.target.iOS17,
        inputs=make_inputs(),
        outputs=[ct.TensorType(name="pooler_output", dtype=np.float32)],
    )


def main() -> None:
    targets = [
        ("stub-bge-base-bad-dtype.mlpackage", build_bad_dtype),
        ("stub-bge-base-bad-shape.mlpackage", build_bad_shape),
        ("stub-bge-base-bad-output-name.mlpackage", build_bad_output_name),
    ]
    for name, builder in targets:
        output_path = os.path.join(SCRIPT_DIR, name)
        model = builder()
        save(model, output_path)
        total = sum(
            os.path.getsize(os.path.join(dp, f))
            for dp, _, fn in os.walk(output_path)
            for f in fn
        )
        print(f"  Size: {total / 1024:.1f} KB")


if __name__ == "__main__":
    main()
