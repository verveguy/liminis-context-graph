#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "graphiti-core[falkordb] @ git+https://github.com/verveguy/graphiti@liminis",
#     "httpx>=0.27",
#     "pytest>=8",
#     "pytest-asyncio>=0.23",
# ]
# ///
"""
End-to-end integration tests for HybridLLMClient + local-inference server.

Runs against the *real* Swift binary. Skips automatically when:
  - The binary hasn't been built yet (run `swift build -c release` first)
  - Apple Intelligence is not available (non-macOS-26 CI environments)

Run with:
    uv run --script native/local-inference/Tests/test_hybrid_llm_integration.py
or (if deps already installed):
    pytest native/local-inference/Tests/test_hybrid_llm_integration.py -v
"""

import asyncio
import json
import os
import signal
import socket
import subprocess
import sys
import textwrap
import time
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock

import httpx
import pytest
import pytest_asyncio

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

PACKAGE_DIR = Path(__file__).parent.parent
# Use the architecture-neutral release path so this works on both Apple Silicon
# and Intel Macs. SwiftPM writes binaries to .build/release/ regardless of host arch.
BINARY = PACKAGE_DIR / ".build" / "release" / "LocalInference"
SOCKET_PATH = "/tmp/liminis-inference-test.sock"

# ---------------------------------------------------------------------------
# HybridLLMClient — copied from graphiti_service.py to avoid importing the
# entire service (which has heavy startup side effects).
# ---------------------------------------------------------------------------

# These imports are available because graphiti-core is in the script deps above.
from graphiti_core.llm_client.config import DEFAULT_MAX_TOKENS, ModelSize
from graphiti_core.llm_client.client import get_extraction_language_instruction
from graphiti_core.prompts.models import Message
from graphiti_core.prompts.dedupe_nodes import NodeDuplicate, NodeResolutions
from graphiti_core.prompts.extract_nodes import (
    ExtractedEntitiesFreeform,
    ExtractedEntityFreeform,
)
from pydantic import BaseModel


class HybridLLMClient:
    """Minimal copy of the production class for testing."""

    _UDS_BASE = "http://local/v1"

    def __init__(self, socket_path: str, local_model: str = "apple-foundation") -> None:
        self._socket_path = socket_path
        self._local_model = local_model
        self._http: httpx.AsyncClient | None = None

    def _get_http(self) -> httpx.AsyncClient:
        if self._http is None:
            transport = httpx.AsyncHTTPTransport(uds=self._socket_path)
            self._http = httpx.AsyncClient(transport=transport, timeout=60.0)
        return self._http

    async def generate_response(
        self,
        messages: list[Message],
        response_model: type[BaseModel] | None = None,
        max_tokens: int = DEFAULT_MAX_TOKENS,
        model_size: ModelSize = ModelSize.medium,
        group_id: str | None = None,
        prompt_name: str | None = None,
    ) -> dict[str, Any]:
        """Only handles the ModelSize.small path (local server)."""
        assert model_size == ModelSize.small, "Test client only exercises the small-model path"

        if response_model is not None:
            schema = json.dumps(response_model.model_json_schema())
            messages[-1].content += (
                f"\n\nRespond with a JSON object in the following format:\n\n{schema}"
            )

        messages[0].content += get_extraction_language_instruction(group_id)

        openai_messages = [{"role": m.role, "content": m.content} for m in messages]
        payload = {
            "model": self._local_model,
            "messages": openai_messages,
            "max_tokens": max_tokens,
            "response_format": {"type": "json_object"},
        }

        http = self._get_http()
        resp = await http.post(f"{self._UDS_BASE}/chat/completions", json=payload)
        resp.raise_for_status()
        data = resp.json()
        content = data["choices"][0]["message"]["content"]
        return json.loads(content)

    async def close(self) -> None:
        if self._http is not None:
            await self._http.aclose()
            self._http = None


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

def _binary_available() -> bool:
    return BINARY.exists() and os.access(BINARY, os.X_OK)


def _wait_for_socket(path: str, timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if Path(path).exists():
            # Also verify it's actually listening (not just the file left over)
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.settimeout(1.0)
                sock.connect(path)
                sock.close()
                return True
            except (ConnectionRefusedError, OSError):
                pass
        time.sleep(0.1)
    return False


@pytest.fixture(scope="module")
def server_process():
    """Start the local-inference binary; skip the whole module if unavailable."""
    if not _binary_available():
        pytest.skip(
            f"Binary not found at {BINARY}. Run `swift build -c release` first."
        )

    # Remove any stale socket
    Path(SOCKET_PATH).unlink(missing_ok=True)

    env = {**os.environ, "LOCAL_INFERENCE_SOCKET": SOCKET_PATH}
    proc = subprocess.Popen(
        [str(BINARY)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    if not _wait_for_socket(SOCKET_PATH, timeout=15.0):
        proc.kill()
        stdout, stderr = proc.communicate(timeout=5)
        pytest.skip(
            "Server did not start in time. "
            f"stdout={stdout.decode()!r} stderr={stderr.decode()!r}. "
            "Apple Intelligence may not be enabled."
        )

    yield proc

    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    Path(SOCKET_PATH).unlink(missing_ok=True)


@pytest_asyncio.fixture
async def client(server_process):
    c = HybridLLMClient(socket_path=SOCKET_PATH)
    yield c
    await c.close()


# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------

def make_messages(system: str, user: str) -> list[Message]:
    return [
        Message(role="system", content=system),
        Message(role="user", content=user),
    ]


# ---------------------------------------------------------------------------
# Tests: raw JSON response
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_plain_json_response(client):
    """Server returns parseable JSON when asked directly."""
    messages = make_messages(
        system="You are a helpful assistant. Always respond with valid JSON.",
        user='Respond with: {"status": "ok"}',
    )
    result = await client.generate_response(
        messages, model_size=ModelSize.small
    )
    assert isinstance(result, dict)


@pytest.mark.asyncio
async def test_response_format_json_object(client):
    """response_format=json_object is sent and the reply is valid JSON."""
    messages = make_messages(
        system="You respond only with JSON.",
        user='Return {"value": 42}',
    )
    result = await client.generate_response(
        messages, model_size=ModelSize.small
    )
    assert isinstance(result, dict)


# ---------------------------------------------------------------------------
# Tests: entity extraction (mirrors Graphiti's add_episode small-model call)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_entity_extraction_schema(client):
    """Server returns a dict matching ExtractedEntitiesFreeform schema."""
    messages = make_messages(
        system="You are an entity extraction assistant.",
        user=textwrap.dedent("""\
            Extract entities from the following text:
            "Marie Curie won the Nobel Prize in Physics in 1903 and later
             founded the Curie Institute in Paris."
        """),
    )
    result = await client.generate_response(
        messages,
        response_model=ExtractedEntitiesFreeform,
        model_size=ModelSize.small,
        prompt_name="extract_entities_freeform",
    )

    assert isinstance(result, dict), f"Expected dict, got {type(result)}: {result}"
    assert "extracted_entities" in result, f"Missing 'extracted_entities' key: {result}"
    entities = result["extracted_entities"]
    assert isinstance(entities, list), f"'extracted_entities' should be a list: {entities}"
    assert len(entities) > 0, "Expected at least one extracted entity"

    # Each entity should have name and entity_type
    for entity in entities:
        assert "name" in entity, f"Entity missing 'name': {entity}"
        assert "entity_type" in entity, f"Entity missing 'entity_type': {entity}"

    names = [e["name"] for e in entities]
    print(f"\n  Extracted entities: {names}")


# ---------------------------------------------------------------------------
# Tests: deduplication (mirrors Graphiti's dedup small-model call)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_entity_dedup_schema(client):
    """Server returns a dict matching NodeResolutions schema."""
    messages = make_messages(
        system=textwrap.dedent("""\
            You are a knowledge graph deduplication assistant.
            Determine whether entity candidates refer to the same real-world entity.
        """),
        user=textwrap.dedent("""\
            Candidate A: "Marie Curie" (person, physicist)
            Candidate B: "Maria Sklodowska-Curie" (person, chemist)

            Are these the same entity? Provide your resolution.
        """),
    )
    result = await client.generate_response(
        messages,
        response_model=NodeResolutions,
        model_size=ModelSize.small,
        prompt_name="dedupe_nodes",
    )

    assert isinstance(result, dict), f"Expected dict, got {type(result)}: {result}"
    assert "entity_resolutions" in result, f"Missing 'entity_resolutions' key: {result}"
    resolutions = result["entity_resolutions"]
    assert isinstance(resolutions, list), f"'entity_resolutions' should be a list: {resolutions}"
    print(f"\n  Dedup resolutions: {resolutions}")


# ---------------------------------------------------------------------------
# Tests: custom Pydantic schema injection
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_custom_schema_injection(client):
    """Schema from a custom Pydantic model is injected and respected."""

    class SentimentResult(BaseModel):
        sentiment: str  # "positive" | "negative" | "neutral"
        confidence: float

    messages = make_messages(
        system="You are a sentiment analysis assistant.",
        user="Classify the sentiment of: 'I absolutely love this!'",
    )
    result = await client.generate_response(
        messages,
        response_model=SentimentResult,
        model_size=ModelSize.small,
    )

    assert isinstance(result, dict)
    assert "sentiment" in result, f"Missing 'sentiment': {result}"
    assert "confidence" in result, f"Missing 'confidence': {result}"
    assert result["sentiment"] in ("positive", "negative", "neutral"), (
        f"Unexpected sentiment value: {result['sentiment']}"
    )
    print(f"\n  Sentiment: {result['sentiment']} (confidence={result['confidence']})")


# ---------------------------------------------------------------------------
# Tests: error / fallback behaviour
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_server_returns_valid_openai_envelope(client):
    """Low-level: raw HTTP response has correct OpenAI-compatible shape."""
    transport = httpx.AsyncHTTPTransport(uds=SOCKET_PATH)
    async with httpx.AsyncClient(transport=transport) as http:
        resp = await http.post(
            "http://local/v1/chat/completions",
            json={
                "model": "apple-foundation",
                "messages": [{"role": "user", "content": "Say 'test'"}],
                "response_format": {"type": "json_object"},
            },
        )
    assert resp.status_code == 200
    data = resp.json()
    assert data["object"] == "chat.completion"
    assert len(data["choices"]) == 1
    assert data["choices"][0]["finish_reason"] == "stop"
    assert data["choices"][0]["message"]["role"] == "assistant"
    assert "usage" in data
    assert data["usage"]["total_tokens"] > 0


@pytest.mark.asyncio
async def test_malformed_request_returns_4xx(server_process):
    """Sending malformed JSON returns a 4xx error."""
    transport = httpx.AsyncHTTPTransport(uds=SOCKET_PATH)
    async with httpx.AsyncClient(transport=transport) as http:
        resp = await http.post(
            "http://local/v1/chat/completions",
            content=b"not json {{{{",
            headers={"content-type": "application/json"},
        )
    assert 400 <= resp.status_code < 500, f"Expected 4xx, got {resp.status_code}"


# ---------------------------------------------------------------------------
# Entrypoint for `uv run --script`
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    import subprocess as _sp
    sys.exit(_sp.call(["pytest", __file__, "-v", "--tb=short"]))
