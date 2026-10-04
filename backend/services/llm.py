from __future__ import annotations

import json
import logging
from typing import List
import requests

from backend.config import OLLAMA_HOST, OLLAMA_MODEL

logger = logging.getLogger("offline_rag.llm")


class LLMService:
    def __init__(self, ollama_host: str = OLLAMA_HOST, ollama_model: str = OLLAMA_MODEL, model_path: str = "") -> None:
        self.ollama_host = ollama_host.rstrip("/")
        self.ollama_model = ollama_model

    def _ensure_model_loaded(self) -> bool:
        """Pings the Ollama service to check if it's available. Automatically starts it if it's offline."""
        import subprocess
        import platform
        import time

        def ping_ollama() -> bool:
            try:
                res = requests.get(f"{self.ollama_host}/api/tags", timeout=1.5)
                return res.status_code == 200
            except Exception:
                return False

        if not ping_ollama():
            logger.info("Ollama is not active at %s. Attempting to start Ollama automatically...", self.ollama_host)
            try:
                model_base = self.ollama_model.split(":")[0]
                if platform.system() == "Windows":
                    # Start minimized command prompt that runs 'ollama run model'
                    subprocess.Popen(
                        ["cmd", "/c", "start", "/min", "ollama", "run", model_base],
                        shell=True,
                        creationflags=subprocess.CREATE_NEW_CONSOLE if hasattr(subprocess, "CREATE_NEW_CONSOLE") else 0
                    )
                else:
                    subprocess.Popen(
                        ["ollama", "run", model_base],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL
                    )

                # Wait up to 8 seconds for the server to spin up and load
                for attempt in range(8):
                    time.sleep(1.0)
                    if ping_ollama():
                        logger.info("Ollama started and verified successfully!")
                        break
            except Exception as e:
                logger.error("Failed to automatically launch Ollama: %s", e)

        # Final verification check
        try:
            res = requests.get(f"{self.ollama_host}/api/tags", timeout=3.0)
            if res.status_code != 200:
                logger.error("Ollama connection active, but tags API returned status %d", res.status_code)
                return False
            
            data = res.json()
            models = [m.get("name") for m in data.get("models", [])]
            target_model = self.ollama_model.lower()
            model_exists = any(
                target_model == m.lower() or target_model.split(":")[0] == m.lower().split(":")[0]
                for m in models
            )
            if not model_exists:
                logger.warning("Model '%s' not explicitly found in Ollama models list: %s.", self.ollama_model, models)
            
            return True
        except Exception as exc:
            logger.error("Ollama connection check failed at %s: %s", self.ollama_host, exc)
            return False

    def _infer_query_type(self, query: str) -> str:
        lower_q = (query or "").lower()
        if any(token in lower_q for token in ["code", "python", "bug", "error", "api", "function"]):
            return "coding"
        if any(token in lower_q for token in ["compare", "analyze", "trade-off", "evaluate", "why"]):
            return "analytical"
        if any(token in lower_q for token in ["write", "story", "creative", "poem", "brainstorm"]):
            return "creative"
        if any(token in lower_q for token in ["hello", "hi", "thanks", "how are you"]):
            return "conversational"
        return "factual"

    def _resolve_output_mode(self, query: str, output_mode: str) -> str:
        if output_mode != "auto":
            return output_mode
        lower_q = (query or "").lower()
        if "json" in lower_q:
            return "json"
        if any(token in lower_q for token in ["table", "tabular"]):
            return "table"
        if any(token in lower_q for token in ["step", "steps", "how to"]):
            return "steps"
        if any(token in lower_q for token in ["code", "python", "script", "function"]):
            return "code"
        return "plain_text"

    def _response_format_block(self, output_mode: str) -> str:
        if output_mode == "json":
            return (
                "Respond with valid JSON only using this schema:\n"
                "{\n"
                '  "answer": "string",\n'
                '  "confidence": "high|medium|low",\n'
                '  "follow_up": "string"\n'
                "}\n"
                "Do not include markdown fences."
            )
        if output_mode == "code":
            return "Prefer concise explanation followed by executable code blocks."
        if output_mode == "steps":
            return "Return a numbered, step-by-step answer."
        if output_mode == "table":
            return "Return a markdown table where practical, then short notes below it."
        return "Return clear plain text with short sections when needed."

    def _build_prompt(self, query: str, contexts: List[str], query_type: str, output_mode: str) -> str:
        context_block = "\n\n".join(contexts[:10])  # Mistral handles larger context, let's allow up to 10 chunks!
        style_rule = {
            "factual": "Focus on factual precision and explicit uncertainty handling.",
            "coding": "Be implementation-focused and include practical snippets when useful.",
            "conversational": "Keep the tone friendly but still grounded in provided context.",
            "analytical": "Compare alternatives, list trade-offs, and justify conclusions.",
            "creative": "Be creative but do not invent unsupported factual claims.",
        }.get(query_type, "Stay accurate, clear, and context-grounded.")
        return (
            "SYSTEM ROLE:\n"
            "You are a helpful, safe, and highly intelligent local assistant.\n\n"
            "BEHAVIOR RULES:\n"
            "- Be accurate and concise unless detail is requested.\n"
            "- Use only the provided context and say when evidence is insufficient.\n"
            "- Avoid fabricated facts.\n"
            f"- {style_rule}\n\n"
            "RESPONSE FORMAT:\n"
            f"{self._response_format_block(output_mode)}\n\n"
            "CONTEXT:\n"
            f"{context_block}\n\n"
            "USER INPUT:\n"
            f"{query}\n\n"
            "ASSISTANT RESPONSE:"
        )

    def generate_answer(
        self,
        query: str,
        contexts: List[str],
        temperature: float = 0.7,
        top_p: float = 0.9,
        generation_top_k: int = 40,
        max_tokens: int = 700,
        frequency_penalty: float = 0.2,
        presence_penalty: float = 0.15,
        query_type: str = "auto",
        output_mode: str = "auto",
    ) -> str:
        if not contexts:
            return "No indexed context was found. Upload documents first."

        if not self._ensure_model_loaded():
            preview = "\n\n".join(contexts[:2])
            return (
                f"Ollama local LLM is not active or model '{self.ollama_model}' is not reachable at {self.ollama_host}.\n"
                "Please run 'ollama run mistral' and restart the backend.\n\n"
                f"Question: {query}\n\n"
                f"Context preview:\n{preview[:1200]}"
            )

        resolved_query_type = query_type if query_type != "auto" else self._infer_query_type(query)
        resolved_output_mode = self._resolve_output_mode(query=query, output_mode=output_mode)
        prompt = self._build_prompt(
            query=query,
            contexts=contexts,
            query_type=resolved_query_type,
            output_mode=resolved_output_mode,
        )

        payload = {
            "model": self.ollama_model,
            "prompt": prompt,
            "stream": False,
            "options": {
                "temperature": temperature,
                "top_p": top_p,
                "top_k": generation_top_k,
                "num_predict": max_tokens,
                "repeat_penalty": 1.0 + min(0.6, (frequency_penalty * 0.12) + (presence_penalty * 0.08)),
            }
        }

        try:
            res = requests.post(f"{self.ollama_host}/api/generate", json=payload, timeout=90.0)
            res.raise_for_status()
            data = res.json()
            decoded = data.get("response", "").strip()

            if resolved_output_mode == "json":
                candidate = decoded
                if "```" in candidate:
                    parts = candidate.split("```")
                    for p in parts:
                        p_strip = p.strip()
                        if p_strip.startswith("json"):
                            p_strip = p_strip[4:].strip()
                        try:
                            parsed = json.loads(p_strip)
                            return json.dumps(parsed, indent=2)
                        except Exception:
                            pass
                try:
                    parsed = json.loads(candidate)
                    return json.dumps(parsed, indent=2)
                except Exception:
                    pass
            return decoded
        except Exception as exc:
            logger.error("Ollama generation failed: %s", exc)
            return f"Error querying local Ollama: {exc}"

    def stream_answer_chunks(
        self,
        query: str,
        contexts: List[str],
        chunk_chars: int = 120,
        temperature: float = 0.7,
        top_p: float = 0.9,
        generation_top_k: int = 40,
        max_tokens: int = 700,
        frequency_penalty: float = 0.2,
        presence_penalty: float = 0.15,
        query_type: str = "auto",
        output_mode: str = "auto",
    ):
        if not contexts:
            yield "No indexed context was found. Upload documents first."
            return

        if not self._ensure_model_loaded():
            preview = "\n\n".join(contexts[:2])
            yield (
                f"Ollama local LLM is not active or model '{self.ollama_model}' is not reachable at {self.ollama_host}.\n"
                "Please run 'ollama run mistral' and restart the backend.\n\n"
                f"Question: {query}\n\n"
                f"Context preview:\n{preview[:1200]}"
            )
            return

        resolved_query_type = query_type if query_type != "auto" else self._infer_query_type(query)
        resolved_output_mode = self._resolve_output_mode(query=query, output_mode=output_mode)
        prompt = self._build_prompt(
            query=query,
            contexts=contexts,
            query_type=resolved_query_type,
            output_mode=resolved_output_mode,
        )

        payload = {
            "model": self.ollama_model,
            "prompt": prompt,
            "stream": True,
            "options": {
                "temperature": temperature,
                "top_p": top_p,
                "top_k": generation_top_k,
                "num_predict": max_tokens,
                "repeat_penalty": 1.0 + min(0.6, (frequency_penalty * 0.12) + (presence_penalty * 0.08)),
            }
        }

        try:
            res = requests.post(f"{self.ollama_host}/api/generate", json=payload, stream=True, timeout=90.0)
            res.raise_for_status()

            buffer = ""
            for line in res.iter_lines():
                if not line:
                    continue
                try:
                    data = json.loads(line.decode("utf-8"))
                    piece = data.get("response", "")
                    buffer += piece
                    if len(buffer) >= chunk_chars or "\n\n" in buffer:
                        yield buffer
                        buffer = ""
                except Exception:
                    continue

            if buffer:
                yield buffer
        except Exception as exc:
            logger.error("Ollama streaming failed: %s", exc)
            yield f"\n[Streaming error: {exc}]"
