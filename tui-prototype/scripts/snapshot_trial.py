#!/usr/bin/env python3
"""Copy a read-only snapshot of the running backend into data/trial.json.

Only GET requests are made, so the polling trial is not disturbed. Folders come
from the OPML export, or from --opml when the backend has no folders yet. Article content is truncated to keep the file small.

Usage: python3 -I scripts/snapshot_trial.py [--api http://127.0.0.1:3000] [--per-feed 40]
"""

import argparse
import json
import pathlib
import urllib.request
import xml.etree.ElementTree as ET

CONTENT_LIMIT = 12_000


def get(api, path):
    with urllib.request.urlopen(api + path, timeout=30) as resp:
        return resp.read()


def folders_by_url(opml):
    root = ET.fromstring(opml)
    folders = {}

    def walk(node, folder):
        for child in node.findall("outline"):
            url = child.get("xmlUrl")
            if url:
                folders[url] = folder
            else:
                walk(child, child.get("text") or child.get("title"))

    walk(root.find("body"), None)
    return folders


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--api", default="http://127.0.0.1:3000")
    parser.add_argument("--opml", default=str(pathlib.Path.home() / "my-feeds.opml"))
    parser.add_argument("--per-feed", type=int, default=40)
    parser.add_argument("--out", default=str(pathlib.Path(__file__).parent.parent / "data" / "trial.json"))
    args = parser.parse_args()

    folders = folders_by_url(get(args.api, "/v1/exports/opml"))
    if not any(folders.values()) and pathlib.Path(args.opml).exists():
        folders = folders_by_url(pathlib.Path(args.opml).read_bytes())
    feeds = json.loads(get(args.api, "/v1/feeds"))["feeds"]
    items = []
    for feed in feeds:
        feed["folder"] = folders.get(feed["url"])
        page = json.loads(get(args.api, f"/v1/items?feed_id={feed['id']}&limit={min(args.per_feed, 100)}"))
        for summary in page["items"][: args.per_feed]:
            detail = json.loads(get(args.api, f"/v1/items/{summary['id']}"))
            for key in ("content", "summary"):
                if detail.get(key) and len(detail[key]) > CONTENT_LIMIT:
                    detail[key] = detail[key][:CONTENT_LIMIT]
            items.append(detail)
        print(f"{feed['title'] or feed['url']}: {len(page['items'][: args.per_feed])} items")

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({"source": "trial", "feeds": feeds, "items": items}, indent=1))
    print(f"wrote {len(feeds)} feeds, {len(items)} items to {out}")


if __name__ == "__main__":
    main()
