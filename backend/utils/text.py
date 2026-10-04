from __future__ import annotations

import re
import unicodedata
from typing import List


def normalize_text(text: str) -> str:
    """
    Cleans raw document text, normalizing Unicode artifacts, catching encoding anomalies,
    and translating typographical glyphs (like smart quotes) to ASCII-compatible forms.
    """
    if not text:
        return ""

    # Normalize unicode to decomposed form (NFKD) to break down composites
    text = unicodedata.normalize("NFKD", text)

    # Standardize common typographic and smart characters to standard ASCII forms
    replacements = {
        "\u201c": '"',  # Left double curly quote
        "\u201d": '"',  # Right double curly quote
        "\u2018": "'",  # Left single curly quote
        "\u2019": "'",  # Right single curly quote
        "\u2013": "-",  # En dash
        "\u2014": "-",  # Em dash
        "\u2212": "-",  # Mathematical minus sign
        "\u2026": "...",  # Horizontal ellipsis
        "\u00a0": " ",  # Non-breaking space
    }
    for orig, repl in replacements.items():
        text = text.replace(orig, repl)

    # Filter out control characters (like backspace, null bytes)
    # but preserve normal whitespace formatting (newline, tab)
    safe_chars = []
    for ch in text:
        category = unicodedata.category(ch)
        if ch in ("\n", "\r", "\t") or not category.startswith("C"):
            safe_chars.append(ch)

    sanitized = "".join(safe_chars)

    # Collapse multiple consecutive whitespaces (including duplicate newlines) to simple single spaces
    cleaned = re.sub(r"\s+", " ", sanitized).strip()
    return cleaned


def chunk_text(text: str, chunk_size: int = 800, overlap: int = 120) -> List[str]:
    if not text:
        return []

    step = max(1, chunk_size - overlap)
    chunks: List[str] = []

    for start in range(0, len(text), step):
        part = text[start : start + chunk_size].strip()
        if part:
            chunks.append(part)

    return chunks
