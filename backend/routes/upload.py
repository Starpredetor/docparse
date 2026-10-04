from __future__ import annotations

import json
from pathlib import Path
from typing import List

from fastapi import APIRouter, File, Form, HTTPException, UploadFile, BackgroundTasks

import logging
from backend.config import CHUNKS_DIR, METADATA_DIR
from backend.models.schemas import UploadBatchResponse, UploadItemResult
from backend.services.embedding import EmbeddingService
from backend.services.ingestion import IngestionService
from backend.services.retrieval import RetrievalService
from backend.services.workspace import WorkspaceStore
from backend.services.knowledge_graph import KnowledgeGraphService

logger = logging.getLogger("offline_rag.upload")


def get_router(
    ingestion_service: IngestionService,
    embedding_service: EmbeddingService,
    retrieval_service: RetrievalService,
    raw_docs_dir: Path,
    workspace_store: WorkspaceStore,
    knowledge_graph_service: KnowledgeGraphService,
) -> APIRouter:
    router = APIRouter(prefix="/upload", tags=["upload"])

    def _safe_stem(name: str) -> str:
        return "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in name)

    def _persist_processed_artifacts(project_id: str, filename: str, chunks: list[str], metadata: list[dict]) -> None:
        safe_project_id = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in project_id)
        project_chunks_dir = CHUNKS_DIR / safe_project_id
        project_meta_dir = METADATA_DIR / safe_project_id
        project_chunks_dir.mkdir(parents=True, exist_ok=True)
        project_meta_dir.mkdir(parents=True, exist_ok=True)

        stem = _safe_stem(Path(filename).stem)
        chunk_file = project_chunks_dir / f"{stem}.jsonl"
        meta_file = project_meta_dir / f"{stem}.json"

        with chunk_file.open("w", encoding="utf-8") as handle:
            for idx, text in enumerate(chunks):
                handle.write(json.dumps({"chunk_index": idx, "text": text}, ensure_ascii=False) + "\n")

        payload = {
            "filename": filename,
            "chunk_count": len(chunks),
            "items": metadata,
        }
        meta_file.write_text(json.dumps(payload, indent=2, ensure_ascii=False), encoding="utf-8")

    @router.post("", response_model=UploadBatchResponse)
    async def upload_document(
        background_tasks: BackgroundTasks,
        files: List[UploadFile] = File(...),
        project_id: str = Form(default="default"),
    ) -> UploadBatchResponse:
        safe_project_id = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in project_id)
        project_raw_dir = raw_docs_dir / safe_project_id
        project_raw_dir.mkdir(parents=True, exist_ok=True)

        if not files:
            raise HTTPException(status_code=400, detail="No files were provided")

        results: list[UploadItemResult] = []
        succeeded = 0

        try:
            for file in files:
                target_path = project_raw_dir / file.filename

                try:
                    data = await file.read()
                    target_path.write_bytes(data)

                    chunks, metadata = ingestion_service.ingest_file(target_path)
                    if not chunks:
                        results.append(
                            UploadItemResult(
                                filename=file.filename,
                                status="failed",
                                message="No text could be extracted",
                            )
                        )
                        continue

                    _persist_processed_artifacts(project_id, file.filename, chunks, metadata)

                    vectors = embedding_service.embed_documents(chunks)
                    retrieval_service.add_chunks(project_id=project_id, vectors=vectors, chunk_metadata=metadata)

                    # Extract entity-relationship triples for the knowledge graph in the background
                    try:
                        full_text = "\n".join(chunks)
                        background_tasks.add_task(
                            knowledge_graph_service.extract_and_merge_graph,
                            project_id,
                            full_text
                        )
                        logger.info("Knowledge Graph: Scheduled background relation extraction for %s.", file.filename)
                    except Exception as kg_exc:
                        logger.exception("Failed to schedule knowledge graph background task for file %s: %s", file.filename, kg_exc)

                    succeeded += 1
                    workspace_store.add_document(
                        project_id=project_id,
                        filename=file.filename,
                        chunks_added=len(chunks),
                        status="success",
                    )
                    results.append(
                        UploadItemResult(
                            filename=file.filename,
                            chunks_added=len(chunks),
                            status="success",
                            message="File ingested and indexed successfully",
                        )
                    )
                except Exception as file_exc:
                    workspace_store.add_document(
                        project_id=project_id,
                        filename=file.filename,
                        chunks_added=0,
                        status="failed",
                    )
                    results.append(
                        UploadItemResult(
                            filename=file.filename,
                            status="failed",
                            message=str(file_exc),
                        )
                    )

            return UploadBatchResponse(
                total_files=len(files),
                succeeded=succeeded,
                failed=len(files) - succeeded,
                items=results,
            )
        except HTTPException:
            raise
        except Exception as exc:
            raise HTTPException(status_code=500, detail=str(exc)) from exc

    return router
