// memory-box plugin untuk OpenCode V2 (server part).
//
// Klien tipis ke memory-box server (Rust). Mendaftarkan:
//  - hook "context"  : injeksi konteks codebase+memory otomatis (hemat token)
//  - hook "prompt"   : capture pertanyaan user → episodic memory
//  - hook "compaction": snapshot ringkasan → semantic memory
//  - tools memory.*  : search / save / graph
//
// Catatan implementasi (dari node_modules/@opencode/plugin + @opencode/schema 2.3.0):
//  - Plugin = { id, setup }; Plugin.define hanyalah identity → default export
//    objek polos agar tidak butuh resolv modul (box ini tanpa node).
//  - SessionPrompt.prompt = { text, ... } (PromptInput.Prompt).
//  - SessionContext = { system: SystemPart[], messages: Message[], options }.
//    SystemPart = { type: "text", text }.
//  - Message user/assistant: { role, content: ContentPart[] }; part teks
//    = { type: "text", text }.
//  - Tool.Info = { name, input: JSONSchema, description, execute(input, ctx),
//    options: { namespace?, codemode? } }; Result = { content?: string, ... }.
//
// Semua jaringan gagal-aman: kegagalan server tidak pernah memutus sesi.

const SERVER_DEFAULT = "http://127.0.0.1:7412"
const INJECT_CHAR_BUDGET = 4000
const FETCH_TIMEOUT_MS = 5000

async function post(url, body, timeoutMs = FETCH_TIMEOUT_MS) {
  try {
    const res = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(timeoutMs),
    })
    if (!res.ok) return null
    return await res.json()
  } catch {
    return null
  }
}

/** Part teks bisa di `text` (PromptInput), `content[]` (Message), atau `parts`. */
function extractText(msg) {
  if (!msg || typeof msg !== "object") return ""
  if (typeof msg.text === "string") return msg.text
  const list = Array.isArray(msg.content) ? msg.content : Array.isArray(msg.parts) ? msg.parts : []
  return list
    .map((p) => (p && typeof p.text === "string" ? p.text : ""))
    .filter(Boolean)
    .join(" ")
}

function lastUserText(messages) {
  if (!Array.isArray(messages)) return ""
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m && (m.role === "user" || m.type === "user")) {
      const t = extractText(m).trim()
      if (t) return t
    }
  }
  return ""
}

export default {
  id: "memory-box",
  async setup(ctx) {
    const opts = ctx.options ?? {}
    const server =
      (typeof opts.server === "string" && opts.server) ||
      process.env.MEMORYBOX_URL ||
      SERVER_DEFAULT
    const autoInject = opts.autoInject !== false

    const directory =
      (typeof opts.projectPath === "string" && opts.projectPath) ||
      process.env.MEMORYBOX_PROJECT ||
      ctx.location?.project?.canonical ||
      ctx.location?.directory ||
      ""
    if (!directory) return

    // buka project di server (idempotent)
    const opened = await post(`${server}/v1/projects/open`, { path: directory })
    const projectID = opened?.project_id
    if (!projectID) {
      console.warn("[memory-box] server tidak tersedia; plugin pasif")
      return
    }
    console.log(`[memory-box] project ${projectID} (${directory})`)

    const retrieve = async (query, k) =>
      post(`${server}/v1/retrieve`, { project_id: projectID, query, k })

    const remember = async (kind, text, metadata) =>
      post(
        `${server}/v1/memory/write`,
        { project_id: projectID, kind, text, metadata },
        8000,
      )

    /** Distilasi transkrip → pelajaran prosedural (self-learning skill). */
    const distill = async (tail) => {
      // model distill bisa diarahkan via env (format providerID/modelID)
      // agar tidak tergantung model sesi — penting saat kuota model
      // sesi habis atau model sesi terlalu besar untuk tugas ini.
      const raw = String(globalThis.process?.env?.MEMORYBOX_DISTILL_MODEL ?? "").trim()
      const modelOpt = raw.includes("/") && !raw.startsWith("/")
        ? (() => {
            const i = raw.indexOf("/")
            return { providerID: raw.slice(0, i), id: raw.slice(i + 1) }
          })()
        : undefined
      try {
        // MODE MOCK: uji seluruh rantai (compaction → tulis → injeksi)
        // tanpa LLM — berguna saat kuota generate habis. Output tetap
        // berbentuk bullet agar jalur penyimpanan identik dengan mode LLM.
        if (String(globalThis.process?.env?.MEMORYBOX_DISTILL_MODE ?? "") === "mock") {
          const lines = tail
            .split("\n")
            .map((l) => l.trim())
            .filter((l) => l.length > 20 && !l.startsWith("#"))
            .slice(0, 6)
            .map((l) => "- " + l.slice(0, 240))
          const text = lines.join("\n").slice(0, 2000)
          if (!text) return { saved: false, text }
          await remember("procedural", text, { source: "compaction-distill-mock", ts: Date.now() })
          return { saved: true, text }
        }
        // runtime 2.0.24: ctx.experimental hanya punya terminal;
        // generate.text tersedia sebagai ctx.generate.text (alias top-level).
        const gen = ctx.experimental?.generate?.text ?? ctx.generate?.text
        if (!gen) return { saved: false, error: "generate.text tidak tersedia di runtime" }
        const g = await gen({
          ...(modelOpt ? { model: modelOpt } : {}),
          prompt:
            "Kamu mengekstrak 'pelajaran' dari sesi coding untuk memory jangka panjang.\n" +
            "Dari transkrip berikut, tulis HANYA keputusan/pola/langkah yang bisa dipakai ulang di masa depan.\n" +
            "Format: bullet singkat (maksimal 6 baris), tanpa basa-basi. " +
            "Jika tidak ada yang berharga, tulis persis: NONE\n\n" +
            "Transkrip:\n" + tail.slice(0, 6000),
        })
        const text = (g?.text ?? "").trim()
        if (!text || text === "NONE" || text.toUpperCase().includes("NONE")) {
          return { saved: false, text }
        }
        await remember("procedural", text.slice(0, 2000), {
          source: "compaction-distill",
          ts: Date.now(),
        })
        return { saved: true, text }
      } catch (e) {
        return { saved: false, error: String(e) + " model=" + JSON.stringify(modelOpt) }
      }
    }

    // ---- hook: prompt → episodic ----
    await ctx.session.hook("prompt", (event) => {
      const text = event.prompt?.text?.trim()
      if (text && text.length > 8) {
        void remember("episodic", text.slice(0, 2000))
      }
    })

    // ---- hook: context → injeksi otomatis ----
    if (autoInject) {
      await ctx.session.hook("context", async (event) => {
        const query = lastUserText(event.messages)
        if (!query) return
        // konteks codebase + pelajaran prosedural yang tersimpan
        const [res, proc] = await Promise.all([
          retrieve(query, 3),
          post(`${server}/v1/memory/search`, {
            project_id: projectID,
            kind: "procedural",
            query,
            k: 2,
          }),
        ])
        const hits = res?.hits ?? []
        const lessons = (proc?.hits ?? []).map((h) => h.text)
        if (!hits.length && !lessons.length) return
        let budget = INJECT_CHAR_BUDGET
        const lines = []
        for (const h of hits) {
          const head = `# ${h.path}:${h.start_line ?? 0}\n${h.content}`
          if (head.length > budget) break
          budget -= head.length + 2
          lines.push(head)
        }
        if (lessons.length && budget > 400) {
          lines.push(
            "# Pelajaran tersimpan (memory-box, hasil distilasi sesi sebelumnya)\n" +
              lessons.map((t) => "- " + t).join("\n"),
          )
        }
        if (!lines.length) return
        event.system.push({
          type: "text",
          text:
            `<memory-box project="${projectID}">\n` +
            "Konteks codebase yang relevan (hasil retrieval; verifikasi sebelum pakai):\n\n" +
            lines.join("\n\n") +
            "\n</memory-box>",
        })
      })
    }

    // ---- hook: compaction → semantic + distilasi prosedural ----
    await ctx.session.hook("compaction", (event) => {
      try {
        const tail = (event.messages ?? [])
          .slice(-8)
          .map((m) => extractText(m))
          .filter(Boolean)
          .join("\n")
        if (tail.length > 32) {
          void remember("semantic", tail.slice(0, 4000), { source: "compaction" })
          void distill(tail)
        }
      } catch {
        // gagal-aman
      }
    })

    // ---- tools ----
    await ctx.tool.transform((editor) => {
      editor.namespace({
        name: "memory",
        description: "Pencarian codebase & memory jangka panjang (memory-box server)",
      })

      editor.add({
        name: "search",
        description:
          "Cari konteks codebase + memory via hybrid retrieval (FTS + vector + RRF). PAKAI INI SEBELUM grep/read berulang.",
        input: {
          type: "object",
          properties: {
            query: { type: "string", description: "pertanyaan atau kata kunci" },
            k: { type: "integer", description: "jumlah hasil (default 5)" },
          },
          required: ["query"],
          additionalProperties: false,
        },
        options: { namespace: "memory", codemode: true },
        execute: async (input) => {
          const { query, k } = input
          const res = await retrieve(query, Math.min(Math.max(k ?? 5, 1), 20))
          if (!res) return { content: "memory-box server tidak merespons" }
          const hits = res.hits ?? []
          if (!hits.length) return { content: "tidak ada hasil" }
          const body = hits
            .map((h) => `## ${h.path}:${h.start_line ?? 0}\n${h.content}`)
            .join("\n\n")
          return { content: body }
        },
      })

      editor.add({
        name: "save",
        description: "Simpan fakta/keputusan/langkah ke memory jangka panjang",
        input: {
          type: "object",
          properties: {
            text: { type: "string", description: "fakta/keputusan/langkah" },
            kind: {
              type: "string",
              description: "episodic | semantic | procedural (default semantic)",
            },
          },
          required: ["text"],
          additionalProperties: false,
        },
        options: { namespace: "memory", codemode: true },
        execute: async (input) => {
          const { text, kind } = input
          const res = await remember(kind ?? "semantic", text)
          return { content: res ? `tersimpan: ${JSON.stringify(res)}` : "gagal simpan" }
        },
      })

      editor.add({
        name: "distill",
        description:
          "Distil teks/keputusan/pola dari sesi menjadi pelajaran prosedural yang tersimpan otomatis di memory jangka panjang (self-learning).",
        input: {
          type: "object",
          properties: {
            text: { type: "string", description: "transkrip/keputusan/pola yang mau didistil" },
          },
          required: ["text"],
          additionalProperties: false,
        },
        options: { namespace: "memory", codemode: true },
        execute: async (input) => {
          const { text } = input
          if (!text || text.length < 32) return { content: "terlalu pendek untuk didistil" }
          const out = await distill(text)
          return {
            content: out.saved
              ? `tersimpan sebagai pelajaran:\n${out.text}`
              : `tidak tersimpan (${out.error ?? "kosong/NONE"}): ${out.text ?? ""}`,
          }
        },
      })

      editor.add({
        name: "graph",
        description:
          "Ambil subgraph konteks di sekitar simbol: siapa memanggil apa, dependensi, dan chunk kodenya. Seed bisa nama simbol (mis. \"scan\") atau \"symbols/<id>\".",
        input: {
          type: "object",
          properties: {
            seed: { type: "string", description: "nama simbol (mis. scan) atau collection/id" },
            hops: { type: "integer", description: "jarak lompatan (default 2, max 8)" },
          },
          required: ["seed"],
          additionalProperties: false,
        },
        options: { namespace: "memory", codemode: true },
        execute: async (input) => {
          const { seed, hops } = input
          let resolved = seed
          if (!seed.includes("/")) {
            // seed = nama simbol → resolve ke symbols/<id>
            const lk = await post(`${server}/v1/symbols/lookup`, {
              project_id: projectID,
              name: seed,
              k: 5,
            })
            const hit = lk?.hits?.[0]
            if (!hit) return { content: `simbol "${seed}" tidak ditemukan` }
            resolved = `symbols/${hit.id}`
          }
          const res = await post(`${server}/v1/graph/context`, {
            project_id: projectID,
            seed: resolved,
            hops,
          })
          if (!res) return { content: "memory-box server tidak merespons" }
          // node bisa simbol/chunk; hop via koleksi edge disajikan sebagai
          // node edge-doc yang membawa _from/_to ({"__ref__": "col/id"}).
          const nodes = res.nodes ?? []
          const docById = {}
          for (const n of nodes) docById[`${n.collection}/${n.id}`] = n.doc ?? {}
          const deref = (x) =>
            x && typeof x === "object" && typeof x.__ref__ === "string" ? x.__ref__ : x
          const nameOf = (ref) => {
            const doc = docById[ref]
            if (!doc) return ref
            const col = String(ref).split("/")[0]
            if (col === "symbols")
              return `${doc.name ?? ""} [${doc.kind ?? "?"}] ${doc.path ?? ""}:${doc.start_line ?? ""}`
            if (col === "chunks") return `${doc.path ?? ""}:${doc.start_line ?? 0}`
            return ref
          }
          const lines = []
          for (const e of res.edges ?? []) {
            const fd = docById[e.from]
            if (fd && fd._from) {
              lines.push(
                `${nameOf(deref(fd._from))} -> ${nameOf(deref(fd._to))}${fd.kind ? ` (${fd.kind})` : ""}`,
              )
            } else {
              lines.push(`${nameOf(e.from)} -> ${nameOf(e.to)}`)
            }
          }
          const header = `subgraph dari ${resolved}: ${nodes.length} node, ${lines.length} edge`
          return { content: `${header}\n${lines.join("\n")}` }
        },
      })

      // [UJI] pemicu compaction manual (aktif hanya dengan MEMORYBOX_TEST_TOOLS=1)
      if (String(globalThis.process?.env?.MEMORYBOX_TEST_TOOLS ?? "") === "1") {
        editor.add({
          name: "compactionTest",
          description: "[UJI] Memicu compaction sesi secara manual untuk menguji hook compaction memory-box.",
          input: { type: "object", properties: {}, additionalProperties: false },
          options: { namespace: "memory", codemode: true },
          execute: async (_input, context) => {
            await ctx.session.compact({ sessionID: context.sessionID })
            return { content: "compaction diminta" }
          },
        })
      }
    })
  },
}
