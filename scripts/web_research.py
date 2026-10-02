#!/usr/bin/env python3
"""web_research: Tavily-powered web search for research tasks.

Usage:
    python3 scripts/web_research.py "your query" [--max-results N]
        [--depth basic|advanced] [--include-domains a.com,b.com]

Requires TAVILY_API_KEY in the environment (or a project .env file).
"""

import argparse
import json
import os
import sys
import urllib.request

API_URL = "https://api.tavily.com/search"


def load_env_file(path: str = ".env") -> None:
    if not os.path.exists(path):
        return
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.partition("=")
            os.environ.setdefault(key.strip(), value.strip())


def search(query: str, max_results: int, depth: str, include_domains: str | None) -> dict:
    api_key = os.environ.get("TAVILY_API_KEY")
    if not api_key:
        sys.exit("error: TAVILY_API_KEY is not set (env or .env)")
    payload = {
        "api_key": api_key,
        "query": query,
        "max_results": max_results,
        "search_depth": depth,
        "include_answer": True,
    }
    if include_domains:
        payload["include_domains"] = [d.strip() for d in include_domains.split(",") if d.strip()]
    req = urllib.request.Request(
        API_URL,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("query")
    parser.add_argument("--max-results", type=int, default=8)
    parser.add_argument("--depth", choices=["basic", "advanced"], default="advanced")
    parser.add_argument("--include-domains", default=None)
    args = parser.parse_args()

    load_env_file()
    result = search(args.query, args.max_results, args.depth, args.include_domains)

    if result.get("answer"):
        print("## Answer")
        print(result["answer"])
        print()

    for i, item in enumerate(result.get("results", []), 1):
        print(f"[{i}] {item.get('title', '(no title)')}")
        print(f"    {item.get('url')}")
        content = (item.get("content") or "").strip()
        if content:
            print(f"    {content[:1200]}")
        print()


if __name__ == "__main__":
    main()
