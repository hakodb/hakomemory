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

    const directory = ctx.location?.project?.canonical ?? ctx.location?.directory ?? ""
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

    const remember = async (kind, text) =>
      post(`${server}/v1/memory/write`, { project_id: projectID, kind, text }, 8000)

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
        const res = await retrieve(query, 5)
        const hits = res?.hits ?? []
        if (!hits.length) return
        let budget = INJECT_CHAR_BUDGET
        const lines = []
        for (const h of hits) {
          const head = `# ${h.path}:${h.start_line ?? 0}\n${h.content}`
          if (head.length > budget) break
          budget -= head.length + 2
          lines.push(head)
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

    // ---- hook: compaction → semantic ----
    await ctx.session.hook("compaction", (event) => {
      try {
        const tail = (event.messages ?? [])
          .slice(-6)
          .map((m) => extractText(m))
          .filter(Boolean)
          .join("\n")
        if (tail.length > 16) {
          void remember("semantic", tail.slice(0, 4000))
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
        name: "graph",
        description:
          "Ambil subgraph konteks di sekitar seed (collection/id), mis. simbol terkait dan dependensinya",
        input: {
          type: "object",
          properties: {
            seed: { type: "string", description: "collection/id, mis. symbols/nama" },
            hops: { type: "integer", description: "jarak lompatan (default 2, max 8)" },
          },
          required: ["seed"],
          additionalProperties: false,
        },
        options: { namespace: "memory", codemode: true },
        execute: async (input) => {
          const { seed, hops } = input
          const res = await post(`${server}/v1/graph/context`, {
            project_id: projectID,
            seed,
            hops,
          })
          if (!res) return { content: "memory-box server tidak merespons" }
          return { content: JSON.stringify(res) }
        },
      })
    })
  },
}
