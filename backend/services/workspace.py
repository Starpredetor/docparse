from __future__ import annotations

import json
import time
from pathlib import Path
from threading import Lock
from typing import Any, Dict, List
from uuid import uuid4


class WorkspaceStore:
    def __init__(self, data_file: Path) -> None:
        self.data_file = data_file
        self.lock = Lock()
        self._state = self._load_or_init()

    def _default_state(self) -> Dict[str, Any]:
        ts = time.time()
        return {
            "projects": [
                {
                    "id": "default",
                    "name": "Default Project",
                    "created_at": ts,
                }
            ],
            "project_docs": {"default": []},
            "project_notebooks": {"default": []},
            "chat_history": [],
            "metrics": {
                "query_count": 0,
                "cache_hits": 0,
                "total_query_latency_ms": 0.0,
            },
        }

    def _load_or_init(self) -> Dict[str, Any]:
        self.data_file.parent.mkdir(parents=True, exist_ok=True)
        if not self.data_file.exists():
            state = self._default_state()
            self._write(state)
            return state

        try:
            state = json.loads(self.data_file.read_text(encoding="utf-8"))
            if not isinstance(state, dict):
                raise ValueError("Invalid workspace state")
        except Exception:
            state = self._default_state()
            self._write(state)

        # Ensure required keys exist after schema changes.
        state.setdefault("projects", self._default_state()["projects"])
        state.setdefault("project_docs", {"default": []})
        state.setdefault("project_notebooks", {"default": []})
        state.setdefault("chat_history", [])
        state.setdefault(
            "metrics",
            {
                "query_count": 0,
                "cache_hits": 0,
                "total_query_latency_ms": 0.0,
            },
        )

        self._ensure_default_project(state)
        self._write(state)
        return state

    def _write(self, state: Dict[str, Any]) -> None:
        self.data_file.write_text(json.dumps(state, indent=2), encoding="utf-8")

    def _ensure_default_project(self, state: Dict[str, Any]) -> None:
        project_ids = {p.get("id") for p in state.get("projects", [])}
        if "default" not in project_ids:
            state["projects"].insert(
                0,
                {
                    "id": "default",
                    "name": "Default Project",
                    "created_at": time.time(),
                },
            )
        state.setdefault("project_docs", {}).setdefault("default", [])
        state.setdefault("project_notebooks", {}).setdefault("default", [])

    def _next_project_id(self, name: str) -> str:
        safe = "".join(ch.lower() if ch.isalnum() else "-" for ch in name).strip("-")
        safe = safe[:48] or "project"
        candidate = safe
        existing = {p["id"] for p in self._state["projects"]}
        suffix = 1
        while candidate in existing:
            suffix += 1
            candidate = f"{safe}-{suffix}"
        return candidate

    def list_projects(self) -> List[Dict[str, Any]]:
        with self.lock:
            enriched = []
            for p in self._state["projects"]:
                project_id = p["id"]
                docs = self._state["project_docs"].get(project_id, [])
                doc_count = len(docs)
                chunk_count = sum(doc.get("chunks_added", 0) for doc in docs)
                enriched.append({
                    **p,
                    "document_count": doc_count,
                    "chunk_count": chunk_count,
                })
            return enriched

    def get_project(self, project_id: str) -> Dict[str, Any] | None:
        with self.lock:
            for item in self._state["projects"]:
                if item["id"] == project_id:
                    return dict(item)
        return None

    def create_project(self, name: str) -> Dict[str, Any]:
        with self.lock:
            project_id = self._next_project_id(name=name)
            project = {
                "id": project_id,
                "name": name.strip(),
                "created_at": time.time(),
            }
            self._state["projects"].append(project)
            self._state["project_docs"][project_id] = []
            self._state["project_notebooks"][project_id] = []
            self._write(self._state)
            return dict(project)

    def add_document(self, project_id: str, filename: str, chunks_added: int, status: str) -> None:
        with self.lock:
            self._state["project_docs"].setdefault(project_id, [])
            self._state["project_docs"][project_id].append(
                {
                    "id": str(uuid4()),
                    "filename": filename,
                    "chunks_added": chunks_added,
                    "status": status,
                    "uploaded_at": time.time(),
                }
            )
            self._write(self._state)

    def list_documents(self, project_id: str) -> List[Dict[str, Any]]:
        with self.lock:
            docs = self._state["project_docs"].get(project_id, [])
            return list(docs)

    def delete_document(self, project_id: str, document_id: str) -> bool:
        filename_to_delete = None
        with self.lock:
            docs = self._state["project_docs"].get(project_id, [])
            original_len = len(docs)
            for item in docs:
                if item.get("id") == document_id:
                    filename_to_delete = item.get("filename")
                    break

            docs = [item for item in docs if item.get("id") != document_id]
            if len(docs) == original_len:
                return False

            self._state["project_docs"][project_id] = docs
            self._write(self._state)

        if filename_to_delete:
            safe_id = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in project_id)
            
            # Delete raw file
            try:
                from backend.config import RAW_DOCS_DIR
                raw_file = RAW_DOCS_DIR / safe_id / filename_to_delete
                if raw_file.exists():
                    raw_file.unlink()
            except Exception:
                pass
                
            # Delete chunk jsonl
            try:
                from backend.config import CHUNKS_DIR
                safe_stem = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in Path(filename_to_delete).stem)
                chunk_file = CHUNKS_DIR / safe_id / f"{safe_stem}.jsonl"
                if chunk_file.exists():
                    chunk_file.unlink()
            except Exception:
                pass

            # Delete metadata json
            try:
                from backend.config import METADATA_DIR
                safe_stem = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in Path(filename_to_delete).stem)
                meta_file = METADATA_DIR / safe_id / f"{safe_stem}.json"
                if meta_file.exists():
                    meta_file.unlink()
            except Exception:
                pass

        return True

    def create_notebook(self, project_id: str, name: str) -> Dict[str, Any]:
        with self.lock:
            self._state["project_notebooks"].setdefault(project_id, [])
            notebook = {
                "id": str(uuid4()),
                "name": name.strip() or "Untitled Notebook",
                "content": "",
                "updated_at": time.time(),
            }
            self._state["project_notebooks"][project_id].append(notebook)
            self._write(self._state)
            return dict(notebook)

    def list_notebooks(self, project_id: str) -> List[Dict[str, Any]]:
        with self.lock:
            return list(self._state["project_notebooks"].get(project_id, []))

    def update_notebook(self, project_id: str, notebook_id: str, content: str) -> Dict[str, Any] | None:
        with self.lock:
            notebooks = self._state["project_notebooks"].get(project_id, [])
            for notebook in notebooks:
                if notebook["id"] == notebook_id:
                    notebook["content"] = content
                    notebook["updated_at"] = time.time()
                    self._write(self._state)
                    return dict(notebook)
        return None

    def add_chat(
        self,
        project_id: str,
        query: str,
        answer: str,
        latency_ms: float,
        source_count: int,
        used_cache: bool,
    ) -> None:
        with self.lock:
            self._state["chat_history"].append(
                {
                    "id": str(uuid4()),
                    "project_id": project_id,
                    "query": query,
                    "answer": answer,
                    "source_count": source_count,
                    "latency_ms": round(latency_ms, 2),
                    "used_cache": bool(used_cache),
                    "created_at": time.time(),
                }
            )
            # Keep bounded history.
            self._state["chat_history"] = self._state["chat_history"][-400:]

            self._state["metrics"]["query_count"] += 1
            self._state["metrics"]["total_query_latency_ms"] += float(latency_ms)
            if used_cache:
                self._state["metrics"]["cache_hits"] += 1

            self._write(self._state)

    def recent_chats(self, limit: int = 12, project_id: str | None = None) -> List[Dict[str, Any]]:
        with self.lock:
            chats = self._state["chat_history"]
            if project_id:
                chats = [item for item in chats if item.get("project_id") == project_id]
            return list(reversed(chats[-max(1, min(limit, 100)) :]))

    def metrics(self, index_size: int) -> Dict[str, Any]:
        with self.lock:
            docs_total = sum(len(v) for v in self._state["project_docs"].values())
            project_total = len(self._state["projects"])
            query_count = int(self._state["metrics"].get("query_count", 0))
            cache_hits = int(self._state["metrics"].get("cache_hits", 0))
            total_latency = float(self._state["metrics"].get("total_query_latency_ms", 0.0))

            avg_latency = (total_latency / query_count) if query_count else 0.0
            cache_hit_rate = ((cache_hits / query_count) * 100.0) if query_count else 0.0

            return {
                "projects": project_total,
                "uploaded_documents": docs_total,
                "indexed_chunks": int(index_size),
                "queries": query_count,
                "cache_hits": cache_hits,
                "cache_hit_rate": round(cache_hit_rate, 2),
                "avg_query_latency_ms": round(avg_latency, 2),
            }

    def delete_project(self, project_id: str) -> bool:
        if project_id == "default":
            return False

        with self.lock:
            # 1. Remove from projects list
            original_len = len(self._state["projects"])
            self._state["projects"] = [p for p in self._state["projects"] if p["id"] != project_id]
            if len(self._state["projects"]) == original_len:
                return False

            # 2. Remove document listings and notebooks
            self._state["project_docs"].pop(project_id, None)
            self._state["project_notebooks"].pop(project_id, None)

            # 3. Clean up chat history
            self._state["chat_history"] = [c for c in self._state["chat_history"] if c.get("project_id") != project_id]

            self._write(self._state)

        import shutil
        safe_id = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in project_id)

        # 4. Clean up Knowledge Graph JSON file
        try:
            from backend.config import KNOWLEDGE_GRAPH_DIR
            graph_file = KNOWLEDGE_GRAPH_DIR / f"{safe_id}.json"
            if graph_file.exists():
                graph_file.unlink()
        except Exception:
            pass

        # 5. Clean up FAISS vector store directory
        try:
            from backend.config import VECTOR_DIR
            project_vector_dir = VECTOR_DIR / safe_id
            if project_vector_dir.exists():
                shutil.rmtree(project_vector_dir, ignore_errors=True)
        except Exception:
            pass

        # 6. Clean up raw documents directory
        try:
            from backend.config import RAW_DOCS_DIR
            project_raw_dir = RAW_DOCS_DIR / safe_id
            if project_raw_dir.exists():
                shutil.rmtree(project_raw_dir, ignore_errors=True)
        except Exception:
            pass

        # 7. Clean up chunk files directory
        try:
            from backend.config import CHUNKS_DIR
            project_chunks_dir = CHUNKS_DIR / safe_id
            if project_chunks_dir.exists():
                shutil.rmtree(project_chunks_dir, ignore_errors=True)
        except Exception:
            pass

        # 8. Clean up metadata files directory
        try:
            from backend.config import METADATA_DIR
            project_meta_dir = METADATA_DIR / safe_id
            if project_meta_dir.exists():
                shutil.rmtree(project_meta_dir, ignore_errors=True)
        except Exception:
            pass

        return True
