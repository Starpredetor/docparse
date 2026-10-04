from __future__ import annotations

import json
from pathlib import Path
from typing import Dict, List
from threading import Lock

import faiss
import numpy as np


class RetrievalService:
    def __init__(self, vector_dir: Path, embedding_dim: int = 384) -> None:
        self.vector_dir = vector_dir
        self.vector_dir.mkdir(parents=True, exist_ok=True)
        self.embedding_dim = embedding_dim
        
        # In-memory index and metadata caching per project
        self._indices: Dict[str, faiss.IndexFlatIP] = {}
        self._metadata: Dict[str, List[Dict]] = {}
        self._locks: Dict[str, Lock] = {}
        self._global_lock = Lock()

    def _get_project_dir(self, project_id: str) -> Path:
        # Standardize project ID to prevent directory traversal
        safe_id = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in project_id)
        project_dir = self.vector_dir / safe_id
        project_dir.mkdir(parents=True, exist_ok=True)
        return project_dir

    def _ensure_project_loaded(self, project_id: str) -> None:
        with self._global_lock:
            if project_id not in self._locks:
                self._locks[project_id] = Lock()

        with self._locks[project_id]:
            if project_id in self._indices:
                return

            project_dir = self._get_project_dir(project_id)
            index_path = project_dir / "index.faiss"
            meta_path = project_dir / "metadata.json"

            if index_path.exists():
                try:
                    self._indices[project_id] = faiss.read_index(str(index_path))
                except Exception:
                    self._indices[project_id] = faiss.IndexFlatIP(self.embedding_dim)
            else:
                self._indices[project_id] = faiss.IndexFlatIP(self.embedding_dim)

            if meta_path.exists():
                try:
                    self._metadata[project_id] = json.loads(meta_path.read_text(encoding="utf-8"))
                except Exception:
                    self._metadata[project_id] = []
            else:
                self._metadata[project_id] = []

    def get_index_size(self, project_id: str) -> int:
        self._ensure_project_loaded(project_id)
        with self._locks[project_id]:
            return int(self._indices[project_id].ntotal)

    def get_total_chunks(self) -> int:
        """Sums up the chunks indexed across all projects currently persisted."""
        total = 0
        try:
            for path in self.vector_dir.glob("*/metadata.json"):
                try:
                    meta = json.loads(path.read_text(encoding="utf-8"))
                    if isinstance(meta, list):
                        total += len(meta)
                except Exception:
                    pass
        except Exception:
            pass
        return total

    def add_chunks(self, project_id: str, vectors: np.ndarray, chunk_metadata: List[Dict]) -> None:
        if vectors.size == 0:
            return

        self._ensure_project_loaded(project_id)
        with self._locks[project_id]:
            index = self._indices[project_id]
            meta = self._metadata[project_id]

            index.add(vectors.astype(np.float32))
            meta.extend(chunk_metadata)

            # Persist project specific files
            project_dir = self._get_project_dir(project_id)
            faiss.write_index(index, str(project_dir / "index.faiss"))
            project_dir.joinpath("metadata.json").write_text(
                json.dumps(meta, indent=2, ensure_ascii=False), encoding="utf-8"
            )

    def search(self, project_id: str, query_vector: np.ndarray, top_k: int = 5) -> List[Dict]:
        self._ensure_project_loaded(project_id)
        with self._locks[project_id]:
            index = self._indices[project_id]
            meta = self._metadata[project_id]

            if index.ntotal == 0:
                return []

            query = np.expand_dims(query_vector, axis=0).astype(np.float32)
            scores, indices = index.search(query, top_k)

            results: List[Dict] = []
            for score, idx in zip(scores[0], indices[0]):
                if idx < 0 or idx >= len(meta):
                    continue

                item = dict(meta[idx])
                item["score"] = float(score)
                item["chunk_id"] = int(idx)
                results.append(item)

            return results
