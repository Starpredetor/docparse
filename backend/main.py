from __future__ import annotations

from pathlib import Path
import sys
import time
from threading import Thread

if __package__ in (None, ""):
    # Supports `python backend/main.py` by adding project root to module path.
    sys.path.append(str(Path(__file__).resolve().parents[1]))

from fastapi import FastAPI
from fastapi import Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles

from backend.config import EMBEDDING_MODEL_DIR, RAW_DOCS_DIR, VECTOR_DIR, VISION_MODEL_DIR, WARMUP_MODELS_ON_STARTUP, WORKSPACE_STATE_FILE, ensure_directories
from backend.logging_config import configure_logging
from backend.runtime import configure_runtime
from backend.routes.query import get_router as get_query_router
from backend.routes.upload import get_router as get_upload_router
from backend.routes.workspace import get_router as get_workspace_router
from backend.services.embedding import EmbeddingService
from backend.services.ingestion import IngestionService
from backend.services.llm import LLMService
from backend.services.retrieval import RetrievalService
from backend.services.vision import VisionService
from backend.services.workspace import WorkspaceStore
from backend.services.knowledge_graph import KnowledgeGraphService

BASE_DIR = Path(__file__).resolve().parents[1]

configure_logging(BASE_DIR)
configure_runtime()

embedding_service = EmbeddingService(model_name=str(EMBEDDING_MODEL_DIR))
retrieval_service = RetrievalService(vector_dir=VECTOR_DIR)
vision_service = VisionService(model_path=str(VISION_MODEL_DIR))
ingestion_service = IngestionService(vision_service=vision_service)
llm_service = LLMService()
knowledge_graph_service = KnowledgeGraphService()
workspace_store = WorkspaceStore(data_file=WORKSPACE_STATE_FILE)

app = FastAPI(title="Offline Multimodal RAG API", version="0.1.0")

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


def _warmup_models() -> None:
    import logging

    logger = logging.getLogger("offline_rag.warmup")
    start_ts = time.perf_counter()
    try:
        embedding_service._load_model()
        llm_service._ensure_model_loaded()
        elapsed = time.perf_counter() - start_ts
        logger.info("model warmup complete | elapsed_sec=%.2f", elapsed)
    except Exception:
        logger.exception("model warmup failed")


@app.on_event("startup")
def startup_warmup() -> None:
    if WARMUP_MODELS_ON_STARTUP:
        Thread(target=_warmup_models, daemon=True).start()


@app.middleware("http")
async def log_requests(request: Request, call_next):
    import logging

    logger = logging.getLogger("offline_rag.api")
    start_ts = time.perf_counter()
    response = await call_next(request)
    elapsed = time.perf_counter() - start_ts
    logger.info(
        "request | method=%s | path=%s | status=%s | elapsed_sec=%.3f",
        request.method,
        request.url.path,
        response.status_code,
        elapsed,
    )
    return response

app.include_router(
    get_upload_router(
        ingestion_service=ingestion_service,
        embedding_service=embedding_service,
        retrieval_service=retrieval_service,
        raw_docs_dir=RAW_DOCS_DIR,
        workspace_store=workspace_store,
        knowledge_graph_service=knowledge_graph_service,
    )
)
app.include_router(
    get_query_router(
        embedding_service=embedding_service,
        retrieval_service=retrieval_service,
        llm_service=llm_service,
        workspace_store=workspace_store,
        knowledge_graph_service=knowledge_graph_service,
    )
)
app.include_router(
    get_workspace_router(
        workspace_store=workspace_store,
        retrieval_service=retrieval_service,
        knowledge_graph_service=knowledge_graph_service,
    )
)

FRONTEND_DIR = BASE_DIR / "frontend_web"
if FRONTEND_DIR.exists():
    app.mount("/ui", StaticFiles(directory=str(FRONTEND_DIR), html=True), name="ui")


@app.get("/app")
def frontend_app() -> FileResponse:
    return FileResponse(FRONTEND_DIR / "index.html")


@app.get("/")
def health() -> dict:
    return {"status": "ok", "service": "offline-multimodal-rag"}


if __name__ == "__main__":
    import uvicorn

    uvicorn.run("backend.main:app", host="127.0.0.1", port=8000, reload=False)
