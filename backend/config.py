from __future__ import annotations

import shutil
import os
from pathlib import Path

ROOT_DIR = Path(__file__).resolve().parents[1]

DATA_DIR = ROOT_DIR / "data"
RAW_DOCS_DIR = DATA_DIR / "raw_docs"
PROCESSED_DIR = DATA_DIR / "processed"
CHUNKS_DIR = PROCESSED_DIR / "chunks"
METADATA_DIR = PROCESSED_DIR / "metadata"
KNOWLEDGE_GRAPH_DIR = PROCESSED_DIR / "knowledge_graph"
VECTOR_DIR = DATA_DIR / "vector_store"
WORKSPACE_STATE_FILE = DATA_DIR / "workspace_state.json"

MODELS_DIR = ROOT_DIR / "models"
EMBEDDING_MODEL_DIR = MODELS_DIR / "embeddings" / "all-MiniLM-L6-v2"
VISION_MODEL_DIR = MODELS_DIR / "vision" / "blip-image-captioning-base"
WARMUP_MODELS_ON_STARTUP = os.getenv("WARMUP_MODELS_ON_STARTUP", "1") == "1"

# Ollama settings
OLLAMA_HOST = os.getenv("OLLAMA_HOST", "http://127.0.0.1:11434")
OLLAMA_MODEL = os.getenv("OLLAMA_MODEL", "mistral:latest")


def ensure_directories() -> None:
    for path in [
        RAW_DOCS_DIR,
        CHUNKS_DIR,
        METADATA_DIR,
        KNOWLEDGE_GRAPH_DIR,
        VECTOR_DIR,
        MODELS_DIR / "embeddings",
        MODELS_DIR / "vision",
    ]:
        path.mkdir(parents=True, exist_ok=True)


def reset_runtime_storage() -> None:
    """Remove uploaded documents and generated retrieval artifacts for a fresh backend run."""
    for path in [RAW_DOCS_DIR, PROCESSED_DIR, VECTOR_DIR]:
        if path.exists():
            shutil.rmtree(path, ignore_errors=True)

    ensure_directories()
