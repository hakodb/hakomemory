#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Evaluasi penghematan token memory-box vs baseline grep/read.

Metode:
  - N pertanyaan realistis tentang repo memory-box + jawaban yang diharapkan
    (path:line yang relevan).
  - memory-box : POST /v1/retrieve k=5; hitung karakter terinjeksi + hit@5
    (path yang diharapkan muncul di hasil).
  - baseline grep  : grep -rn <keyword> (keyword khas dari pertanyaan);
    konteks = baris match + 3 baris sekitar, digandakan x2 (model biasanya
    grep 2x sebelum menemukan yang tepat).
  - baseline read  : model membaca file penuh tempat jawaban berada.
  - Estimasi token: chars / 4 (aproksimasi umum).

Output: tabel per pertanyaan + total, disimpan juga ke eval-results.json.
"""

import json
import os
import subprocess
import urllib.request

BASE = os.environ.get("MEMORYBOX_URL", "http://127.0.0.1:7412")
REPO = os.environ.get(
    "MEMORYBOX_PROJECT",
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
)
PROJECT = REPO

def pid():
    req = urllib.request.Request(
        BASE + "/v1/projects/open",
        data=json.dumps({"path": PROJECT}).encode(),
        headers={"Content-Type": "application/json"},
    )
    return json.load(urllib.request.urlopen(req))["project_id"]

def retrieve(pid_, query, k=5):
    req = urllib.request.Request(
        BASE + "/v1/retrieve",
        data=json.dumps({"project_id": pid_, "query": query, "k": k}).encode(),
        headers={"Content-Type": "application/json"},
    )
    return json.load(urllib.request.urlopen(req))["hits"]

# (pertanyaan, path yang diharapkan di top-5, keyword grep yang khas)
CASES = [
    ("bagaimana embedder melakukan mean pooling dan normalisasi L2?", "crates/hakomemory-embed/src/lib.rs", "pooling"),
    ("di mana skema koleksi dan index dibuat?", "crates/hakomemory-server/src/store.rs", "create_vector_index"),
    ("bagaimana chunking per baris dibatasi ukurannya?", "crates/hakomemory-server/src/indexer.rs", "MAX_CHUNK_LINES"),
    ("bagaimana hybrid search RRF dipanggil untuk retrieval kode?", "crates/hakomemory-server/src/api.rs", "hybrid_search"),
    ("bagaimana watcher melakukan debounce event inotify?", "crates/hakomemory-server/src/watcher.rs", "DEBOUNCE"),
    ("bagaimana distilasi pelajaran dipanggil saat kompaksi?", "plugins/memory-box/index.ts", "distill"),
    ("bagaimana resolusi callee di call-graph memilih kandidat?", "crates/hakomemory-server/src/indexer.rs", "by_name"),
    ("bagaimana HakoDB dibuka per project dan kenapa urutannya penting?", "crates/hakomemory-server/src/store.rs", "open_project"),
    ("bagaimana hook context menyisipkan konteks ke system prompt?", "plugins/memory-box/index.ts", "memory-box project"),
    ("bagaimana cleanup simbol stale bekerja saat file berubah?", "crates/hakomemory-server/src/indexer.rs", "stale_deletes"),
    ("bagaimana adapter MCP merender subgraph edge-doc?", "crates/hakomemory-server/src/bin/hakomemory-mcp.rs", "deref"),
    ("bagaimana projek id dihitung dari path?", "crates/hakomemory-server/src/store.rs", "project_id"),
]

def tok(chars):
    return int(chars / 4)

def grep_baseline(keyword, x2=2):
    """grep -rn + konteks 3 baris, x2 percobaan grep."""
    try:
        pr = subprocess.run(
            ["grep", "-rn", "--include=*.rs", "--include=*.ts", "-C", "3", keyword, REPO],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30
        )
        out = pr.stdout.decode("utf-8", "replace")
    except Exception:
        out = ""
    return len(out) * x2

def read_baseline(path):
    try:
        with open(REPO + "/" + path) as f:
            return len(f.read())
    except Exception:
        return 0

def run(cases):
    p = pid()
    META = ("PLAN.md", "docs/PLAN.md", "README.md", "eval-results.json", "scripts/", "plugins/memory-box/README.md")
    rows = []
    for q, expected, kw in cases:
        hits = retrieve(p, q, 5)
        code_hits = [h for h in hits if not any(h["path"].startswith(m) or ("/" + m) in h["path"] for m in META)]
        paths = [h["path"] for h in code_hits]
        hit = any(expected in pp for pp in paths)
        mb_chars = sum(len(h["content"]) for h in code_hits)
        g_chars = grep_baseline(kw)
        r_chars = read_baseline(expected)
        all_paths = [h["path"] for h in hits]
        hit_all = any(expected in pp for pp in all_paths)
        rows.append({
            "q": q,
            "hit@5": hit,
            "hit_all": hit_all,
            "mb_chars": mb_chars,
            "grep_chars": g_chars,
            "read_chars": r_chars,
            "top": paths[0] if paths else "-",
        })

    n = len(rows)
    hits_n = sum(r["hit@5"] for r in rows)
    hits_all = sum(r["hit_all"] for r in rows)
    mb_c = sum(r["mb_chars"] for r in rows)
    g_c = sum(r["grep_chars"] for r in rows)
    r_c = sum(r["read_chars"] for r in rows)

    print(f"{'pertanyaan':<58} {'hit@5':>5} {'mb':>7} {'grep':>7} {'read':>7}")
    for r in rows:
        print(f"{r['q'][:56]:<58} {str(r['hit@5']):>5} {r['mb_chars']:>7} {r['grep_chars']:>7} {r['read_chars']:>7}")
    print()
    print(f"hit@5 (kode saja)     : {hits_n}/{n}")
    print(f"hit@5 (termasuk docs) : {hits_all}/{n}")
    print(f"chars memory-box      : {mb_c:>7} (~{tok(mb_c)} tokens)")
    print(f"chars baseline grep*2 : {g_c:>7} (~{tok(g_c)} tokens)")
    print(f"chars baseline read   : {r_c:>7} (~{tok(r_c)} tokens)")
    print(f"hemat vs grep         : {100 - tok(mb_c) * 100 // max(tok(g_c), 1)}%")
    print(f"hemat vs read         : {100 - tok(mb_c) * 100 // max(tok(r_c), 1)}%")

    with open("/tmp/mb-eval-results.json", "w") as f:
        json.dump({"rows": rows, "totals": {"hit": hits_n, "n": n, "mb": mb_c, "grep": g_c, "read": r_c}}, f, indent=1)
    print("tersimpan: /tmp/mb-eval-results.json")

EN_CASES = [
    ("how does the embedder do mean pooling and L2 normalization?", "crates/hakomemory-embed/src/lib.rs", "pooling"),
    ("where are the collection schema and indexes created?", "crates/hakomemory-server/src/store.rs", "create_vector_index"),
    ("how is line chunking bounded in size?", "crates/hakomemory-server/src/indexer.rs", "MAX_CHUNK_LINES"),
    ("how is hybrid RRF search called for code retrieval?", "crates/hakomemory-server/src/api.rs", "hybrid_search"),
    ("how does the watcher debounce inotify events?", "crates/hakomemory-server/src/watcher.rs", "DEBOUNCE"),
    ("how is the lesson distillation invoked during compaction?", "plugins/memory-box/index.ts", "distill"),
    ("how does call-graph callee resolution pick candidates?", "crates/hakomemory-server/src/indexer.rs", "by_name"),
    ("how is HakoDB opened per project and why does order matter?", "crates/hakomemory-server/src/store.rs", "open_project"),
    ("how does the context hook inject context into the system prompt?", "plugins/memory-box/index.ts", "memory-box project"),
    ("how does stale symbol cleanup work when a file changes?", "crates/hakomemory-server/src/indexer.rs", "stale_deletes"),
    ("how does the MCP adapter render edge-doc subgraphs?", "crates/hakomemory-server/src/bin/hakomemory-mcp.rs", "deref"),
    ("how is the project id computed from a path?", "crates/hakomemory-server/src/store.rs", "project_id"),
]

def main():
    print("=== SET INDONESIA ===")
    run(CASES)
    print()
    print("=== SET INGGRIS (kontrol bahasa MiniLM) ===")
    run(EN_CASES)

if __name__ == "__main__":
    main()
