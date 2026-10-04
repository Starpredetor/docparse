from __future__ import annotations

import logging
import time
from collections import OrderedDict, deque
from threading import Lock, Thread
from typing import Deque, Dict, List, Tuple
from uuid import uuid4

from fastapi import APIRouter, HTTPException

from backend.models.schemas import QueryChunkResponse, QueryRequest, QueryResponse, QueryStartResponse, SourceChunk
from backend.services.embedding import EmbeddingService
from backend.services.llm import LLMService
from backend.services.retrieval import RetrievalService
from backend.services.workspace import WorkspaceStore
from backend.services.knowledge_graph import KnowledgeGraphService

logger = logging.getLogger("offline_rag.query")


class _JobState:
    def __init__(self, query: str, top_k: int, sources: List[SourceChunk]) -> None:
        self.query = query
        self.top_k = top_k
        self.queue: Deque[str] = deque()
        self.full_answer = ""
        self.done = False
        self.error: str | None = None
        self.sources = sources
        self.updated_at = time.time()


class _ResponseCacheItem:
    def __init__(self, answer: str, sources: List[SourceChunk]) -> None:
        self.answer = answer
        self.sources = sources
        self.updated_at = time.time()


def get_router(
    embedding_service: EmbeddingService,
    retrieval_service: RetrievalService,
    llm_service: LLMService,
    workspace_store: WorkspaceStore,
    knowledge_graph_service: KnowledgeGraphService,
) -> APIRouter:
    router = APIRouter(prefix="/query", tags=["query"])
    jobs: Dict[str, _JobState] = {}
    jobs_lock = Lock()
    response_cache: OrderedDict[Tuple[str, str, int, int, float, float, int, int, float, float, str, str], _ResponseCacheItem] = OrderedDict()
    response_cache_max = 200

    def _cache_key(request: QueryRequest) -> Tuple[str, str, int, int, float, float, int, int, float, float, str, str]:
        # Include index size so cache invalidates automatically after new uploads.
        return (
            (request.query or "").strip().lower(),
            request.project_id,
            request.top_k,
            retrieval_service.get_index_size(request.project_id),
            round(request.temperature, 3),
            round(request.top_p, 3),
            request.generation_top_k,
            request.max_tokens,
            round(request.frequency_penalty, 3),
            round(request.presence_penalty, 3),
            request.query_type,
            request.output_mode,
        )

    def _get_cached_response(request: QueryRequest) -> _ResponseCacheItem | None:
        key = _cache_key(request)
        item = response_cache.get(key)
        if item is None:
            return None
        response_cache.move_to_end(key)
        return item

    def _set_cached_response(request: QueryRequest, answer: str, sources: List[SourceChunk]) -> None:
        key = _cache_key(request)
        response_cache[key] = _ResponseCacheItem(answer=answer, sources=sources)
        response_cache.move_to_end(key)
        while len(response_cache) > response_cache_max:
            response_cache.popitem(last=False)

    def _prune_jobs(max_age_seconds: int = 900) -> None:
        now = time.time()
        stale_ids = []
        for job_id, state in jobs.items():
            if state.done and (now - state.updated_at) > max_age_seconds:
                stale_ids.append(job_id)
        for job_id in stale_ids:
            jobs.pop(job_id, None)

    def _run_job(job_id: str, request: QueryRequest, contexts: List[str]) -> None:
        start_ts = time.perf_counter()
        try:
            for chunk in llm_service.stream_answer_chunks(
                query=request.query,
                contexts=contexts,
                chunk_chars=request.stream_chunk_chars,
                temperature=request.temperature,
                top_p=request.top_p,
                generation_top_k=request.generation_top_k,
                max_tokens=request.max_tokens,
                frequency_penalty=request.frequency_penalty,
                presence_penalty=request.presence_penalty,
                query_type=request.query_type,
                output_mode=request.output_mode,
            ):
                with jobs_lock:
                    state = jobs.get(job_id)
                    if state is None:
                        return
                    state.queue.append(chunk)
                    state.full_answer += chunk
                    state.updated_at = time.time()
            with jobs_lock:
                state = jobs.get(job_id)
                if state is not None:
                    state.done = True
                    state.updated_at = time.time()
                    if not state.error and state.full_answer.strip():
                        _set_cached_response(request, state.full_answer, state.sources)
                        elapsed_ms = (time.perf_counter() - start_ts) * 1000.0
                        workspace_store.add_chat(
                            project_id=request.project_id,
                            query=request.query,
                            answer=state.full_answer,
                            latency_ms=elapsed_ms,
                            source_count=len(state.sources),
                            used_cache=False,
                        )
        except Exception as exc:
            logger.exception("query job failed | job_id=%s", job_id)
            with jobs_lock:
                state = jobs.get(job_id)
                if state is not None:
                    state.done = True
                    state.error = str(exc)
                    state.updated_at = time.time()
        finally:
            elapsed = time.perf_counter() - start_ts
            logger.info("query job finished | job_id=%s | elapsed_sec=%.2f", job_id, elapsed)

    @router.post("", response_model=QueryResponse)
    async def query_documents(request: QueryRequest) -> QueryResponse:
        try:
            start_ts = time.perf_counter()

            cached = _get_cached_response(request)
            if cached is not None:
                elapsed = time.perf_counter() - start_ts
                workspace_store.add_chat(
                    project_id=request.project_id,
                    query=request.query,
                    answer=cached.answer,
                    latency_ms=elapsed * 1000.0,
                    source_count=len(cached.sources),
                    used_cache=True,
                )
                logger.info(
                    "query sync cache hit | top_k=%s | index_size=%s | elapsed_sec=%.3f",
                    request.top_k,
                    retrieval_service.get_index_size(request.project_id),
                    elapsed,
                )
                return QueryResponse(answer=cached.answer, sources=cached.sources)

            query_vector = embedding_service.embed_query(request.query)
            hits = retrieval_service.search(project_id=request.project_id, query_vector=query_vector, top_k=request.top_k)

            contexts = [item["text"] for item in hits]

            # Inject Knowledge Graph subgraph context
            try:
                kg_context = knowledge_graph_service.get_subgraph_context(
                    project_id=request.project_id,
                    query=request.query
                )
                if kg_context:
                    contexts.insert(0, kg_context)
            except Exception as kg_exc:
                logger.error("Failed to retrieve subgraph context: %s", kg_exc)
            answer = llm_service.generate_answer(
                query=request.query,
                contexts=contexts,
                temperature=request.temperature,
                top_p=request.top_p,
                generation_top_k=request.generation_top_k,
                max_tokens=request.max_tokens,
                frequency_penalty=request.frequency_penalty,
                presence_penalty=request.presence_penalty,
                query_type=request.query_type,
                output_mode=request.output_mode,
            )

            sources = [
                SourceChunk(
                    chunk_id=item["chunk_id"],
                    score=item["score"],
                    text=item["text"],
                    source_file=item["source_file"],
                    page=item.get("page"),
                )
                for item in hits
            ]

            _set_cached_response(request, answer, sources)

            elapsed = time.perf_counter() - start_ts
            workspace_store.add_chat(
                project_id=request.project_id,
                query=request.query,
                answer=answer,
                latency_ms=elapsed * 1000.0,
                source_count=len(sources),
                used_cache=False,
            )
            logger.info(
                "query sync success | top_k=%s | hits=%s | elapsed_sec=%.2f",
                request.top_k,
                len(hits),
                elapsed,
            )
            return QueryResponse(answer=answer, sources=sources)
        except Exception as exc:
            logger.exception("query sync failed")
            raise HTTPException(status_code=500, detail=str(exc)) from exc

    @router.post("/start", response_model=QueryStartResponse)
    async def start_query_stream(request: QueryRequest) -> QueryStartResponse:
        try:
            _prune_jobs()

            cached = _get_cached_response(request)
            if cached is not None:
                job_id = str(uuid4())
                workspace_store.add_chat(
                    project_id=request.project_id,
                    query=request.query,
                    answer=cached.answer,
                    latency_ms=1.0,
                    source_count=len(cached.sources),
                    used_cache=True,
                )
                with jobs_lock:
                    state = _JobState(query=request.query, top_k=request.top_k, sources=cached.sources)
                    chunk_size = 140
                    for i in range(0, len(cached.answer), chunk_size):
                        state.queue.append(cached.answer[i : i + chunk_size])
                    state.full_answer = cached.answer
                    state.done = True
                    state.updated_at = time.time()
                    jobs[job_id] = state
                logger.info("query stream cache hit | job_id=%s | top_k=%s", job_id, request.top_k)
                return QueryStartResponse(job_id=job_id, status="started")

            query_vector = embedding_service.embed_query(request.query)
            hits = retrieval_service.search(project_id=request.project_id, query_vector=query_vector, top_k=request.top_k)
            contexts = [item["text"] for item in hits]

            # Inject Knowledge Graph subgraph context for streaming
            try:
                kg_context = knowledge_graph_service.get_subgraph_context(
                    project_id=request.project_id,
                    query=request.query
                )
                if kg_context:
                    contexts.insert(0, kg_context)
            except Exception as kg_exc:
                logger.error("Failed to retrieve subgraph context in stream: %s", kg_exc)

            sources = [
                SourceChunk(
                    chunk_id=item["chunk_id"],
                    score=item["score"],
                    text=item["text"],
                    source_file=item["source_file"],
                    page=item.get("page"),
                )
                for item in hits
            ]

            job_id = str(uuid4())
            with jobs_lock:
                jobs[job_id] = _JobState(query=request.query, top_k=request.top_k, sources=sources)

            Thread(target=_run_job, args=(job_id, request, contexts), daemon=True).start()
            logger.info("query stream started | job_id=%s | top_k=%s | hits=%s", job_id, request.top_k, len(hits))
            return QueryStartResponse(job_id=job_id, status="started")
        except Exception as exc:
            logger.exception("query stream start failed")
            raise HTTPException(status_code=500, detail=str(exc)) from exc

    @router.get("/next/{job_id}", response_model=QueryChunkResponse)
    async def next_query_chunk(job_id: str, max_chunks: int = 4) -> QueryChunkResponse:
        with jobs_lock:
            state = jobs.get(job_id)
            if state is None:
                raise HTTPException(status_code=404, detail="Invalid or expired job_id")

            collected: List[str] = []
            for _ in range(max(1, min(max_chunks, 20))):
                if not state.queue:
                    break
                collected.append(state.queue.popleft())

            done = state.done and not state.queue
            delta = "".join(collected)
            sources = state.sources if done else []
            error = state.error
            state.updated_at = time.time()

            if done:
                jobs.pop(job_id, None)

        return QueryChunkResponse(job_id=job_id, delta=delta, done=done, sources=sources, error=error)

    return router
