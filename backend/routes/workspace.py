from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from backend.services.retrieval import RetrievalService
from backend.services.workspace import WorkspaceStore
from backend.services.knowledge_graph import KnowledgeGraphService


class ProjectCreateRequest(BaseModel):
    name: str = Field(..., min_length=2, max_length=100)


class NotebookCreateRequest(BaseModel):
    name: str = Field(..., min_length=1, max_length=120)


class NotebookUpdateRequest(BaseModel):
    content: str = Field(default="")


def get_router(
    workspace_store: WorkspaceStore,
    retrieval_service: RetrievalService,
    knowledge_graph_service: KnowledgeGraphService,
) -> APIRouter:
    router = APIRouter(prefix="/workspace", tags=["workspace"])

    @router.get("/projects/{project_id}/graph")
    async def get_project_graph(project_id: str) -> dict:
        if workspace_store.get_project(project_id=project_id) is None:
            raise HTTPException(status_code=404, detail="Project not found")
        graph = knowledge_graph_service.get_graph(project_id=project_id)
        return graph

    @router.get("/dashboard")
    async def dashboard() -> dict:
        return {
            "metrics": workspace_store.metrics(index_size=retrieval_service.get_total_chunks()),
            "projects": workspace_store.list_projects(),
            "recent_chats": workspace_store.recent_chats(limit=10),
        }

    @router.get("/metrics")
    async def metrics() -> dict:
        return workspace_store.metrics(index_size=retrieval_service.get_total_chunks())

    @router.get("/projects")
    async def list_projects() -> dict:
        return {"items": workspace_store.list_projects()}

    @router.post("/projects")
    async def create_project(payload: ProjectCreateRequest) -> dict:
        project = workspace_store.create_project(name=payload.name)
        return {"item": project}

    @router.delete("/projects/{project_id}")
    async def delete_project(project_id: str) -> dict:
        if project_id == "default":
            raise HTTPException(status_code=400, detail="Cannot delete the default notebook")
        deleted = workspace_store.delete_project(project_id=project_id)
        if not deleted:
            raise HTTPException(status_code=404, detail="Notebook not found")
        return {"deleted": True}

    @router.get("/projects/{project_id}")
    async def get_project(project_id: str) -> dict:
        project = workspace_store.get_project(project_id=project_id)
        if project is None:
            raise HTTPException(status_code=404, detail="Project not found")

        return {
            "project": project,
            "documents": workspace_store.list_documents(project_id=project_id),
            "notebooks": workspace_store.list_notebooks(project_id=project_id),
            "recent_chats": workspace_store.recent_chats(limit=10, project_id=project_id),
        }

    @router.get("/projects/{project_id}/documents")
    async def list_project_documents(project_id: str) -> dict:
        return {"items": workspace_store.list_documents(project_id=project_id)}

    @router.delete("/projects/{project_id}/documents/{document_id}")
    async def delete_project_document(project_id: str, document_id: str) -> dict:
        deleted = workspace_store.delete_document(project_id=project_id, document_id=document_id)
        if not deleted:
            raise HTTPException(status_code=404, detail="Document not found")
        return {"deleted": True}

    @router.get("/projects/{project_id}/notebooks")
    async def list_project_notebooks(project_id: str) -> dict:
        return {"items": workspace_store.list_notebooks(project_id=project_id)}

    @router.post("/projects/{project_id}/notebooks")
    async def create_project_notebook(project_id: str, payload: NotebookCreateRequest) -> dict:
        if workspace_store.get_project(project_id=project_id) is None:
            raise HTTPException(status_code=404, detail="Project not found")

        notebook = workspace_store.create_notebook(project_id=project_id, name=payload.name)
        return {"item": notebook}

    @router.put("/projects/{project_id}/notebooks/{notebook_id}")
    async def update_project_notebook(project_id: str, notebook_id: str, payload: NotebookUpdateRequest) -> dict:
        notebook = workspace_store.update_notebook(
            project_id=project_id,
            notebook_id=notebook_id,
            content=payload.content,
        )
        if notebook is None:
            raise HTTPException(status_code=404, detail="Notebook not found")
        return {"item": notebook}

    return router
