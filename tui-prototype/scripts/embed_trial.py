#!/usr/bin/env python3
"""Compute an embedding for every article in a fixture with a local Ollama model.

Writes <fixture>.<model>.embeddings.json next to the fixture: {"model": ..., "items": {id: [floats]}}.
Each article is embedded from its title plus the start of its text.

Usage: python3 -I scripts/embed_trial.py [--model nomic-embed-text] [--data data/trial.json]
"""

import argparse
import html
import json
import pathlib
import re
import time
import urllib.request

TEXT_LIMIT = 2_000
BATCH = 16
# nomic-embed-text was trained with task prefixes; other models take plain text.
PREFIXES = {"nomic-embed-text": "search_document: "}


def plain(markup):
    text = re.sub(r"<[^>]+>", " ", markup or "")
    return " ".join(html.unescape(text).split())


def embed(api, model, texts):
    body = json.dumps({"model": model, "input": texts}).encode()
    req = urllib.request.Request(f"{api}/api/embed", data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as resp:
        return json.load(resp)["embeddings"]


def main():
    root = pathlib.Path(__file__).parent.parent
    parser = argparse.ArgumentParser()
    parser.add_argument("--api", default="http://127.0.0.1:11434")
    parser.add_argument("--model", default="nomic-embed-text")
    parser.add_argument("--data", default=str(root / "data" / "trial.json"))
    args = parser.parse_args()

    items = json.loads(pathlib.Path(args.data).read_text())["items"]
    prefix = PREFIXES.get(args.model, "")
    texts = [
        prefix + f"{i.get('title') or ''}. {plain(i.get('content') or i.get('summary'))}"[:TEXT_LIMIT]
        for i in items
    ]

    start = time.time()
    vectors = []
    for n in range(0, len(texts), BATCH):
        vectors += embed(args.api, args.model, texts[n : n + BATCH])
        print(f"\r{args.model}: {len(vectors)}/{len(texts)}", end="", flush=True)
    print(f"  ({time.time() - start:.0f} s)")

    out = pathlib.Path(args.data).with_suffix(f".{args.model}.embeddings.json")
    data = {"model": args.model, "items": {str(i["id"]): [round(x, 5) for x in v] for i, v in zip(items, vectors)}}
    out.write_text(json.dumps(data, separators=(",", ":")))
    print(f"wrote {out} ({out.stat().st_size // 1024} KB)")


if __name__ == "__main__":
    main()
