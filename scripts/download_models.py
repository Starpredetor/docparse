from __future__ import annotations

from pathlib import Path

from huggingface_hub import snapshot_download

ROOT = Path(__file__).resolve().parents[1]
MODELS_DIR = ROOT / "models"

EMBEDDING_REPO = "sentence-transformers/all-MiniLM-L6-v2"
VISION_REPO = "Salesforce/blip-image-captioning-base"


def download_embedding() -> Path:
    target = MODELS_DIR / "embeddings" / "all-MiniLM-L6-v2"
    target.mkdir(parents=True, exist_ok=True)
    
    # Check if model is already downloaded
    if (target / "config.json").exists() and (target / "pytorch_model.bin").exists() or (target / "model.safetensors").exists():
        print(f"Embedding model already downloaded: {target}")
        return target
    
    snapshot_download(
        repo_id=EMBEDDING_REPO,
        local_dir=str(target),
        local_dir_use_symlinks=False,
    )
    return target


def download_vision() -> Path:
    target = MODELS_DIR / "vision" / "blip-image-captioning-base"
    target.mkdir(parents=True, exist_ok=True)
    
    # Check if model is already downloaded
    if (target / "config.json").exists() and ((target / "pytorch_model.bin").exists() or (target / "model.safetensors").exists()):
        print(f"Vision model already downloaded: {target}")
        return target
    
    snapshot_download(
        repo_id=VISION_REPO,
        local_dir=str(target),
        local_dir_use_symlinks=False,
        allow_patterns=[
            "*.json",
            "*.txt",
            "*.model",
            "*.bin",
            "*.safetensors",
            "tokenizer*",
            "preprocessor_config.json",
            "vocab.txt",
            "special_tokens_map.json",
        ],
    )
    return target


if __name__ == "__main__":
    emb_path = download_embedding()
    print(f"Embedding model ready: {emb_path}")

    vision_path = download_vision()
    print(f"Vision model ready: {vision_path}")
