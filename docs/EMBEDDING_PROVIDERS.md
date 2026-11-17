# Kajian penyedia embedding untuk memory-box

Tanggal: 2026-11-17. Sumber: dokumentasi Google Gemini Embeddings, model card
NVIDIA `llama-nemotron-embed-vl-1b-v2`, Hugging Face OpenBMB.
Konteks: server saat ini memakai `paraphrase-multilingual-MiniLM-L12-v2` via
candle, 384d, lokal, one-forward batch. Terukur: retrieve end-to-end **29–31 ms**
di box ini; recall eval `ID 8/12`, `EN 8/12`.

## 1. Fakta model yang ditanyakan

| Model | Apa itu sebenarnya | Dimensi | Catatan penting |
|---|---|---|---|
| **NVIDIA "nemotron-3-embed-1b"** | Nama di HF adalah `nvidia/llama-nemotron-embed-vl-1b-v2` (text+VLM, 1.7B: Llama 3.2 1B + SigLIP2 400M). Juga ada varian text-only `llama-nemotron-embed-1b-v2`. | **2048** tetap | Dioptimasi untuk **GPU** (H100/A100/L40S/A10G/B200/RTX PRO 6000), butuh `transformers>=4.56` + PyTorch / vLLM / TensorRT. Tersedia sebagai **NIM** (online service). Tidak murni Rust/candle. Input teks 8192 token (image+text 10240). |
| **MiniCPM-V-4.7-1B (OpenBMB)** | `openbmb/MiniCPM-V-4.7-1B-gguf` adalah **VLM chat/image-text-to-text**, bukan embedding model. Tidak ada mode `encode_query`/`encode_document`. | tidak relevan | Memakainya sebagai embedder berarti mengambil hidden state sendiri (pooling custom) — tidak didukung, hasil tidak terjamin. Butuh llama.cpp/GGUF + Python. **Tidak direkomendasikan untuk embedding.** |
| **Google Gemini Embedding** | `gemini-embedding-001` (text-only, 2048 token) dan `gemini-embedding-2` (multimodal, 8192 token, update Apr 2026). | fleksibel **128–3072**; disarankan 768 / 1536 / 3072 | MRL: `output_dimensionality`. MTEB `gemini-embedding-2`: 768→67.99, 1536→68.17, 3072→68.16 (hampir sama). V2 auto-normalisasi; V1 perlu normalisasi manual untuk non-3072. Ada task type/code retrieval. **Berbayar**, butuh API key. Vertex batch: ≤250 teks, ≤20.000 token/request. |

Catatan migrasi penting: `gemini-embedding-001` dan `gemini-embedding-2` **tidak kompatibel** satu sama lain — harus re-embed seluruh data bila pindah. Hal yang sama berlaku untuk pindah dari MiniLM ke model mana pun.

## 2. Perbandingan performa (estimatIFE + data lokal)

| Aspek | MiniLM candle (sekarang) | Gemini Embedding (API) | Nemotron Embed via NIM | MiniCPM-V 4.7-1B |
|---|---|---|---|---|
| Latensi query embed | ~10–20 ms (lokal) | ~100–400 ms (RTT + API) | ~100–400 ms (RTT + NIM) | tidak sesuai |
| Retrieve end-to-end | **29–31 ms** (terukur) | ~120–450 ms | ~120–450 ms | — |
| Indexing 130 chunk | detik, satu forward/batch 16 | beberapa request batch (~5–10), detik | beberapa request batch, detik | — |
| RAM di box | ~470 MB model, CPU | ~0 di box | ~0 di box (jika online) | ~1–2 GB + Python |
| Dimensi vektor | 384 → 384 float ≈1.5 KB/chunk | pilih 768 → 3 KB/chunk | 2048 → 8 KB/chunk (≈5×) | — |
| Biaya | Rp 0 | per token (murah untuk repo kecil) | tergantung NIM | — |
| Privasi kode | penuh (lokal) | kode dikirim ke vendor | kode dikirim ke vendor/endpoint | — |
| Kualitas | ID 8/12, EN 8/12 (terukur) | kemungkinan naik; **harus di-evaluasi** | RTEB tinggi; harus di-evaluasi | tidak relevan |
| Cocok untuk i5 8GB | ✅ | ✅ (server ringan) | ✅ (jika online) | ❌ |

Angka latency API adalah estimasi jaringan, bukan hasil bench di box ini — perlu diukur dengan provider nyata.

## 3. Implikasi skema HakoDB v0.21.0

`ensure_indexes` sekarang **error keras** bila definisi vector berbeda dengan yang tersimpan ("drop it first"). Jadi **jangan** mengubah `DIM` 384 → 768/2048 secara in-place.

Strategi aman:
- Simpan vektor baru di field baru, mis. `emb_gemini768` / `emb_nemotron2048`, dengan index vector baru; patch `store.rs::ensure_schema` menambah def, bukan mengganti yang lama.
- Atau versi koleksi: `chunks_v2`, `mem_*_v2` — paling bersih tapi butuh migrasi data.
- Simpan `model_id` + `dim` di `meta` supaya startup tahu backend aktif.
- Selama migrasi, kedua field boleh hidup berdampingan; retrieval memilih field sesuai konfigurasi aktif.

## 4. Rekomendasi arsitektur

Jangan mengikat memory-box ke vendor. Pakai satu interface (sudah ada `TextEmbedder` di `mb-embed`) dengan tiga backend:

1. **Local candle** (default sekarang) — cepat, gratis, privat.
2. **HTTP self-hosted** — `mb-embed-srv` di box lain / GPU box; endpoint `/embed` + `/dim`; token auth. Ini yang direncanakan untuk memindahkan embedder dari box 8GB i5.
3. **Vendor API adapter** — service kecil terpisah yang menerima format internal memory-box dan menerjemahkan ke Gemini `embedContent` / NIM `/v1/embeddings`, lengkap dengan task prefix, batching, retry, dan normalisasi. Memory-box hanya bicara HTTP ke adapter ini.

Env yang diusulkan: `MEMORYBOX_EMBED_URL`, `MEMORYBOX_EMBED_TOKEN`, `MEMORYBOX_EMBED_DIM`, `MEMORYBOX_EMBED_MODEL`. Kosong = lokal.

## 5. Rekomendasi pilihan

1. **Default tetap MiniLM candle 384d.** Untuk 1–5 project dan codebase menengah, ini paling efisien; tidak ada alasan pindah hanya karena "model lebih besar".
2. **Kalau recall kurang**, uji dulu dengan `scripts/eval-token.py` memakai adapter Gemini `gemini-embedding-001` dengan:
   - query: `CODE_RETRIEVAL_QUERY` / `task: code retrieval | query: ...`
   - doc: `RETRIEVAL_DOCUMENT` / `title: ... | text: ...`
   - `output_dimensionality = 768`
   Bandingkan hit@5 vs baseline 8/12 sebelum commit migrasi.
3. **Nemotron 2048** baru pertimbangkan bila ada akses NIM/GPU dan kebutuhan multimodal (gambar/tabel). Biaya vektor 5× dan mustahil dijalankan lokal di CPU CentOS 7 tanpa Python stack.
4. **MiniCPM-V 4.7-1B jangan dipakai sebagai embedder** — itu VLM generatif, bukan model retrieval.
5. **Lokal LLM vs bebas kuota**: LLM untuk distill tetap di plugin/OpenCode; embedding lokal sudah bebas kuota. Memindahkan embedding ke API justru menambah ketergantungan kuota/biaya — lakukan hanya bila kualitas membuktikan untungnya.

## 6. Rencana implementasi bila disetujui

- Fase A — SUDAH DIEKSEKUSI: `EmbeddingSpec` tersimpan di `meta/embedding`,
  `HttpEmbedder` generik, env `MEMORYBOX_EMBED_*`, validasi mismatch keras.
  Teruji: project 768d end-to-end + remote embedder HTTP 768d end-to-end.
- Fase B: adapter Gemini/NIM terpisah + benchmark `hit@5`, latency, biaya.
- Fase C: migrasi field/index versi baru + backfill, rollback ke 384d kapan saja.
- Fase D: ganti default hanya bila eval menang signifikan.

---

## 7. Update: opsi online 384d & batas dimensi HakoDB (2026-11-17)

### HakoDB v0.21.0
`src/index/vector.rs:82`: `pub const MAX_DIM: u32 = 4096;` dengan validasi
`dim == 0 || dim > MAX_DIM → InvalidInput`. Jadi **384–1024 didukung penuh**.
`hnsw_m_for_dim()` juga menyesuaikan `M` sesuai dimensi (D=384→59, D=4096→64),
jadi tidak ada alasan teknis membatasi memory-box ke 384.

### Online yang bisa 384d (terverifikasi dari docs)

| Provider | Model | Cara 384d | Catatan |
|---|---|---|---|
| **OpenAI** | `text-embedding-3-small` | `dimensions: 384` | MRL shortening; default 1536, max 8192 token. MTEB 62.3. Berbayar per token. |
| **Cohere** | `embed-multilingual-light-v3.0` | **native 384** | Multibahasa termasuk **Indonesia**; max 512 token; lebih cepat/murah dari v3.0 penuh (1024). `embed-english-light-v3.0` juga 384. |
| **Google Gemini** | `gemini-embedding-001` / `-2` | `output_dimensionality: 384` | Docs menyebut rentang 128–3072; 384 tidak dalam rekomendasi (768/1536/3072), jadi **perlu di-evaluasi** kualitasnya. |
| **Jina / lainnya** | vendor MRL | biasanya `dimensions` | Perlu verifikasi per-model; prinsipnya sama dengan OpenAI. |

Catatan: Cohere `embed-v5.0-*` tidak punya 384 (pilihan 256/512/768/1024/1536/2048), tapi `embed-multilingual-light-v3.0` masih tersedia dan **paling cocok** untuk kebutuhan 384d + bahasa Indonesia.

### Kesimpulan revisi

Karena opsi 384d ada (terutama Cohere multilingual-light-v3.0), dan HakoDB
mendukung sampai 4096, rekomendasi最终 adalah:
- **Pertahankan MiniLM lokal sebagai default** (gratis, privat, 29–31 ms).
- **Jadikan memory-box model-agnostik untuk 384–1024**: dimensi bukan konstanta
  kompile, tetapi `EmbeddingSpec { model_id, dim, metric, provider }` yang
  dipersisten di `meta`. `ensure_schema` memakai `spec.dim`.
- Ganti `assert_eq!(embedder.dim(), DIM)` dengan validasi terhadap spec;
  kalau berbeda → error migrasi yang jelas (jangan silent).
- Karena `ensure_indexes` v0.21 error bila def vector berbeda, perubahan dim
  wajib memakai **field/index baru** (`emb_<d>`) atau koleksi versi baru +
  re-embed; jangan menimpa `emb` lama.
- Backend: lokal candle, HTTP self-hosted, atau adapter vendor—semuanya
  lewat `TextEmbedder` yang sudah ada.


## 8. Benchmark nyata: NVIDIA NIM vs MiniLM lokal (2026-11-17)

Kredensial NIM (`nvapi-...`) dipakai sementara untuk benchmark, tidak
disimpan di repo. Model: `nvidia/nemotron-3-embed-1b`, fixed **2048d**
(endpoints NVIDIA tidak menerima `dimensions` selain 2048; adapter `nim`
sengaja tidak mengirim field tersebut). Gemini 384d tidak bisa diselesaikan
karena API key kena HTTP 429 berulang pada batch indexing (satu request
tunggal sukses → masalah rate limit, bukan format).

Kondisi adil: repo sama (44 file, 211 chunk), server fresh, eval 24 soal
(`scripts/eval-token.py`).

| Metrik | MiniLM lokal 384d | NVIDIA NIM 2048d |
|---|---|---|
| hit@5 Indonesia | **9/12** | 8/12 |
| hit@5 Inggris | 8/12 | 8/12 |
| retrieve latency (5x) | **30–31 ms** | 445–575 ms |
| hemat vs grep | 71–73% | 62–64% |
| vektor/chunk | 384 float (1.5 KB) | 2048 float (8 KB) |

Kesimpulan: **NIM tidak menaikkan recall** untuk codebase ini (ID justru
turun 1), latency ≈15–19× lebih lambat karena RTT + API, dan index 5× lebih
besar. Rekomendasi final: **pertahankan MiniLM lokal sebagai default**. NIM
tetap tersedia sebagai opsi lewat `MEMORYBOX_ADAPTER_PROVIDER=nim` bila
suatu hari dibutuhkan(model lain/kualitas berbeda), bukan untuk kasus ini.

Perbaikan yang dihasilkan dari uji ini: retry backoff + throttle global di
adapter, dan env `MEMORYBOX_EMBED_BATCH` (default 16; 96 dipakai saat uji)
untuk menekan jumlah request API.
