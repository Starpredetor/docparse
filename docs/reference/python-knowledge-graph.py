from __future__ import annotations

import json
import logging
import re
from pathlib import Path
from typing import Dict, List, Tuple, Any
from threading import Lock
import requests

from backend.config import KNOWLEDGE_GRAPH_DIR, OLLAMA_HOST, OLLAMA_MODEL

logger = logging.getLogger("offline_rag.knowledge_graph")


class KnowledgeGraphService:
    def __init__(self, ollama_host: str = OLLAMA_HOST, ollama_model: str = OLLAMA_MODEL) -> None:
        self.ollama_host = ollama_host.rstrip("/")
        self.ollama_model = ollama_model
        self.lock = Lock()
        
        # Ensure directories exist
        KNOWLEDGE_GRAPH_DIR.mkdir(parents=True, exist_ok=True)

    def _get_project_file(self, project_id: str) -> Path:
        # Standardize project ID to prevent directory traversal
        safe_id = "".join(c if c.isalnum() or c in ("-", "_") else "_" for c in project_id)
        return KNOWLEDGE_GRAPH_DIR / f"{safe_id}.json"

    def _load_graph(self, project_id: str) -> Dict[str, Any]:
        file_path = self._get_project_file(project_id)
        if not file_path.exists():
            return {"nodes": [], "edges": []}
        
        try:
            return json.loads(file_path.read_text(encoding="utf-8"))
        except Exception as exc:
            logger.error("Failed to load knowledge graph for project %s: %s", project_id, exc)
            return {"nodes": [], "edges": []}

    def _save_graph(self, project_id: str, graph: Dict[str, Any]) -> None:
        file_path = self._get_project_file(project_id)
        try:
            file_path.write_text(json.dumps(graph, indent=2, ensure_ascii=False), encoding="utf-8")
        except Exception as exc:
            logger.error("Failed to save knowledge graph for project %s: %s", project_id, exc)

    def extract_and_merge_graph(self, project_id: str, text: str) -> int:
        """
        Splits text into context-rich sections, uses Ollama Mistral to extract entity relationships
        concurrently across parallel threads, and merges them incrementally into the project's knowledge graph.
        Returns the number of relationships extracted.
        """
        if not text or len(text.strip()) < 50:
            return 0

        # Sectioning text: ~3500 chars (approx. 800-1000 tokens) to preserve semantic scope.
        section_size = 3500
        sections: List[str] = []
        for i in range(0, len(text), section_size):
            sec = text[i:i + section_size].strip()
            if len(sec) > 100:
                sections.append(sec)

        if not sections and text.strip():
            sections.append(text.strip())

        new_triples: List[Dict[str, str]] = []

        # Run extraction in parallel using a ThreadPoolExecutor!
        from concurrent.futures import ThreadPoolExecutor, as_completed
        max_workers = min(4, len(sections)) if len(sections) > 0 else 1
        
        logger.info("Knowledge Graph: Extracting entity-relation triples across %d parallel threads.", max_workers)
        
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            futures = {
                executor.submit(self._query_ollama_for_triples, section): idx
                for idx, section in enumerate(sections)
            }
            
            for future in as_completed(futures):
                idx = futures[future]
                try:
                    triples = future.result()
                    if triples:
                        new_triples.extend(triples)
                        logger.info("Thread completed extraction for section %d/%d (found %d relationships)", idx + 1, len(sections), len(triples))
                except Exception as thread_exc:
                    logger.error("Thread for section %d failed: %s", idx + 1, thread_exc)

        if not new_triples:
            logger.info("No relationships extracted from document.")
            return 0

        logger.info("Merging %d extracted relationship triples into graph for project %s", len(new_triples), project_id)
        
        with self.lock:
            graph = self._load_graph(project_id)
            
            # Index current nodes and edges for fast merging
            nodes_map: Dict[str, Dict[str, Any]] = {n["id"].lower(): n for n in graph.get("nodes", [])}
            edges_map: Dict[Tuple[str, str, str], Dict[str, Any]] = {}
            for e in graph.get("edges", []):
                key = (e["source"].lower(), e["target"].lower(), e["relation"].lower())
                edges_map[key] = e

            # Deduplication & standardized formats
            added_edges_count = 0
            for triple in new_triples:
                source = triple.get("source", "").strip()
                target = triple.get("target", "").strip()
                relation = triple.get("relation", "").strip()

                if not source or not target or not relation:
                    continue
                if source.lower() == target.lower():
                    continue # Skip self-loops

                # Clean names (standard casing)
                source_clean = source.strip()
                target_clean = target.strip()
                relation_clean = relation.strip()

                src_key = source_clean.lower()
                tgt_key = target_clean.lower()
                edge_key = (src_key, tgt_key, relation_clean.lower())

                # Update or insert nodes
                if src_key not in nodes_map:
                    nodes_map[src_key] = {"id": source_clean, "label": source_clean, "type": "Concept", "count": 1}
                else:
                    nodes_map[src_key]["count"] = nodes_map[src_key].get("count", 1) + 1

                if tgt_key not in nodes_map:
                    nodes_map[tgt_key] = {"id": target_clean, "label": target_clean, "type": "Concept", "count": 1}
                else:
                    nodes_map[tgt_key]["count"] = nodes_map[tgt_key].get("count", 1) + 1

                # Update or insert edges
                if edge_key not in edges_map:
                    edges_map[edge_key] = {
                        "source": nodes_map[src_key]["id"],
                        "target": nodes_map[tgt_key]["id"],
                        "relation": relation_clean,
                        "count": 1
                    }
                    added_edges_count += 1
                else:
                    edges_map[edge_key]["count"] = edges_map[edge_key].get("count", 1) + 1

            # Save back to graph structure
            graph["nodes"] = list(nodes_map.values())
            graph["edges"] = list(edges_map.values())
            self._save_graph(project_id, graph)

        return added_edges_count

    def _query_ollama_for_triples(self, text: str) -> List[Dict[str, str]]:
        prompt = (
            "Extract a list of the most important entities and their semantic relationships from the text below.\n"
            "Identify key concepts, organizations, people, locations, events, or technologies.\n\n"
            "Respond ONLY with a valid JSON array of objects, containing exactly these keys: 'source', 'target', and 'relation'.\n"
            "Example format:\n"
            "[\n"
            "  {\"source\": \"FastAPI\", \"target\": \"Python\", \"relation\": \"built on top of\"},\n"
            "  {\"source\": \"Mistral 7B\", \"target\": \"Ollama\", \"relation\": \"runs on\"}\n"
            "]\n\n"
            "Do not write any markdown code fences (like ```json), explanations, notes, or conversational text. Output the JSON array only.\n\n"
            f"Text to analyze:\n{text}"
        )

        payload = {
            "model": self.model_model_check(),
            "prompt": prompt,
            "stream": False,
            "options": {
                "temperature": 0.1,  # Low temperature for highly deterministic, structured extraction
                "num_predict": 1024,
            }
        }

        try:
            res = requests.post(f"{self.ollama_host}/api/generate", json=payload, timeout=60.0)
            if res.status_code != 200:
                logger.error("Ollama extraction returned status code %d", res.status_code)
                return []
            
            raw_response = res.json().get("response", "").strip()
            
            # Clean and extract JSON using regex in case model adds code blocks or wrappers
            json_match = re.search(r"\[([\s\S]*)\]", raw_response)
            if json_match:
                clean_json = json_match.group(0)
                try:
                    data = json.loads(clean_json)
                    if isinstance(data, list):
                        return [item for item in data if isinstance(item, dict)]
                except Exception as parse_err:
                    logger.warning("Failed parsing extracted regex JSON from Ollama: %s | Raw response was: %s", parse_err, raw_response)
            
            # Fallback direct parse
            try:
                data = json.loads(raw_response)
                if isinstance(data, list):
                    return data
            except Exception:
                pass

        except Exception as exc:
            logger.error("Ollama triplet extraction failed: %s", exc)
        
        return []

    def model_model_check(self) -> str:
        return self.ollama_model

    def get_graph(self, project_id: str) -> Dict[str, Any]:
        """Returns the nodes and edges for visual representation."""
        with self.lock:
            return self._load_graph(project_id)

    def get_subgraph_context(self, project_id: str, query: str) -> str:
        """
        Extracts entities from the user's query using exact word matching,
        retrieves matching graph relations (subgraph context),
        and formats them into a structured text context block.
        """
        graph = self.get_graph(project_id)
        nodes = graph.get("nodes", [])
        edges = graph.get("edges", [])

        if not nodes or not edges:
            return ""

        # Normalize query words
        query_words = set(re.findall(r"\b\w{3,15}\b", query.lower()))
        
        # Stopwords to filter out query noise
        stopwords = {
            "the", "and", "for", "with", "from", "that", "this", "what", "how", "why", "who", "when", "where",
            "are", "was", "were", "been", "have", "has", "had", "will", "would", "shall", "should", "can", "could"
        }
        query_keywords = query_words - stopwords

        if not query_keywords:
            return ""

        # Find matching nodes
        matched_node_ids = set()
        for node in nodes:
            node_id_lower = node["id"].lower()
            # If the node entity is mentioned in the query
            if node_id_lower in query.lower() or any(kw in node_id_lower for kw in query_keywords):
                matched_node_ids.add(node["id"])

        if not matched_node_ids:
            return ""

        # Retrieve edges connected to matching nodes (limit to 12 for conciseness)
        relevant_relations = []
        seen_edges = set()
        for edge in edges:
            src = edge["source"]
            tgt = edge["target"]
            rel = edge["relation"]
            
            if src in matched_node_ids or tgt in matched_node_ids:
                edge_key = (src, tgt, rel)
                if edge_key not in seen_edges:
                    seen_edges.add(edge_key)
                    relevant_relations.append(f"- {src} ({edge.get('relation', 'related to')}) {tgt}")
                    if len(relevant_relations) >= 12:
                        break

        if not relevant_relations:
            return ""

        # Build context header
        logger.info("Hybrid Vector-Graph RAG: Injected %d structured graph relationships.", len(relevant_relations))
        context_str = "STRUCTURED KNOWLEDGE GRAPH RELATIONSHIPS:\n" + "\n".join(relevant_relations)
        return context_str
