#!/bin/bash
# Uji end-to-end self-learning memory-box tanpa bergantung kuota LLM.
# Mode mock menggantikan ctx.generate.text dengan ekstraksi bullet deterministik,
# sehingga rantai compaction -> tulis mem_procedural -> injeksi konteks tetap
# diuji penuh. Untuk menguji dengan LLM asli, lihat README/PLAN.
set -euo pipefail
REPO=${MEMORYBOX_PROJECT:-$(cd "$(dirname "$0")/.." && pwd)}
cd "$REPO"
OC=${OC:-$HOME/.opencode/bin/opencode}
SVR=${SVR:-http://127.0.0.1:7412}
PID=${PID:-$(python3 -c 'import hashlib,sys;print(hashlib.sha256(sys.argv[1].encode()).hexdigest()[:12])' "$REPO")}
MARK=${MARK:-"MARKER-UJI-$(date +%s)"}
MODEL=${MODEL:-opencode/inkling-2-small-free}

post() { curl -s -X POST "$SVR$1" -H 'content-type: application/json' -d "$2"; }

echo "== marker: $MARK"
echo "== pastikan project open"
post /v1/projects/open "{\"path\":\"$REPO\"}" >/dev/null

echo "== sesi 1: diskusi + picu compaction (hook -> semantic + distill mock)"
MEMORYBOX_DISTILL_MODE=mock MEMORYBOX_TEST_TOOLS=1 timeout 240 \
  "$OC" run --standalone --model "$MODEL" \
  "Keputusan uji $MARK: selalu verifikasi kebenaran data sebelum mempercayai angka benchmark; baseline BFS 0.20.6 setengah kosong karena race #209. Jelaskan singkat lalu panggil tool memory.compactionTest." \
  | tail -3

sleep 4
echo "== verifikasi mem_procedural"
RES=$(post /v1/memory/search "{\"project_id\":\"$PID\",\"kind\":\"procedural\",\"query\":\"$MARK benchmark race 209\",\"k\":3}")
echo "$RES" | python3 -c "
import json,sys
d=json.load(sys.stdin)
if not d.get('hits'): sys.exit('GAGAL: tidak ada pelajaran prosedural')
print('OK: %d hit, teratas: %s' % (len(d['hits']), d['hits'][0]['text'][:120].replace(chr(10),' | ')))
"

echo "== verifikasi injeksi ke sesi baru"
OUT=$(MEMORYBOX_DISTILL_MODE=mock timeout 240 "$OC" run --standalone --model "$MODEL" \
  "Jangan panggil tool. Apakah system prompt kamu memuat blok 'Pelajaran tersimpan'? Kutip satu baris yang berisi $MARK." 2>&1)
echo "$OUT" | tail -3
if grep -qF "$MARK" <<<"$OUT"; then
  echo "LULUS: marker hasil distill terinjeksi di sesi baru"
else
  echo "GAGAL: marker tidak terlihat terinjeksi"; exit 1
fi
