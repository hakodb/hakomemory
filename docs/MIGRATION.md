# Migration & versioning

## Schema version

`meta/schema_version` stores the data contract version (currently `3`).
Version 2 added generic documents; version 3 added parent-child chunk ids
(`parent_id`) with no re-embedding.
`POST /v1/admin/migrate` with `dry_run: true` reports the version and fields
to backfill; with `dry_run: false` it writes defaults without re-embedding.

## v1 → v2 (documents)

Projects created before generic documents have no `documents`/`files`
collections. On first `documents/upsert` or `scan` the engine writes
`schema_version = 2`, creates the collections and backfills missing
`source_uri`/`metadata` fields on existing documents. Existing agent memory
(`mem_*`) and code graph (`symbols`/`edges`) collections stay untouched.

## Embedding changes

`EmbeddingSpec` persists four attributes: `model_id`, `provider`, `dim`,
`metric`. Opening a project with a different value in **any** of them is
rejected with an explicit re-embed/migration error; vectors are never silently
mixed. To change models:

1. Back up the project directory (`Hako::backup`).
2. Create a new vector field (or reindex into a fresh project directory).
3. Re-embed documents/memory with the new model.
4. Switch clients to the new project.

## Portability

Portable bundle export/import (manifest + embedding spec, with or without
vectors) is not implemented yet; it is tracked in issue #13. Until then, copy
the whole project directory while no writer is active, and keep the same
embedding spec on the destination.
