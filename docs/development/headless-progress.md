# Headless Mode Progress

Working notes for the in-progress headless/API re-implementation. The durable
rules live in `AGENTS.md` (see its "Headless Mode" section); this file tracks
what is done, what is broken right now, and what comes next. Delete each
completed section as it lands upstream.

## Goal

Koharu runs three ways over one shared runtime: desktop (window + server),
headless (local HTTP API, no window), and MCP. Phase plan:

1. Transport-agnostic `App` core in `koharu-app` (Tauri commands become thin
   adapters).
2. New `koharu-rpc` crate: axum REST under `/api/v1` (core workflow only:
   projects, pages, import, process, export, fonts, preferences), SSE events,
   OpenAPI via utoipa, static UI serving, readiness gating (503).
3. Headless entry: `--headless/--host/--port/--config/--store` CLI,
   headless-exclusive `headless.json` with pipeline overlays, first-run model
   auto-install via a `Pipeline::warm(stages)` that calls each stage's
   `StageProcessor::load()` (weights download through the existing
   `HuggingFaceFile::resolve()`/`Store` machinery), `scripts/start_headless.ps1`
   + `.sh`. Windows-first (paths, console attach, no CEF in headless path).
4. Deferred: UI dual transport (HTTP/SSE in `@koharu/bridge` when not in
   Tauri), canvas-manipulation endpoints, MCP layer.

Decisions: headless gets an exclusive `~/.koharu/headless.json` (host, port,
warmup, pipeline model overlays, local translation endpoint) overlaying
`~/.koharu/config.toml`; no credential handling (local providers only);
API-only startup (no auto-open project); OpenAPI spec wanted.

## Phase 1 — done

- `crates/koharu-app/src/core/` is the transport-agnostic home: `App`
  aggregate (project, library, processing, desktop, pipeline `OnceLock`,
  events, initialization, agent `OnceLock`), `EventBus` with typed
  `Event::{Canvas, Job, Download, Resources, Project}`, and the shared DTO
  modules (`project`, `jobs`, `downloads`, `fonts`, `preferences`, `export`,
  `import`) plus their workflows.
- `App::initialize(self: &Arc<Self>)` is the single startup path for both
  modes: `koharu_ml::init` → device/metrics context → `attach_pipeline` →
  resource/download event producers (`tokio::spawn`, no Tauri runtime
  dependency) → restore the current page onto the canvas. Desktop `run()`
  spawns it and calls `mark_ready()`; headless calls it directly.
- `App::process` (moved out of the commands layer) is the shared job runner;
  `commit_inpaint` and the `process` command both go through it.
- `App` extras for the HTTP layer: `is_ready()` (non-blocking probe for the
  503 gate), `page_view(id)`, `export_pages`, `page_thumbnail`,
  `rendered_preview`, `list_fonts`, `font_preview`, `warm`.
- All Tauri commands in `commands/*` are thin adapters over `SharedApp`;
  channel slots (`CanvasChannel`…`ProjectChannel`) and `forward_events` stay
  desktop-only transport state. Agent commands remain Tauri-only for now.
- The startup panic (`pipeline()` before `attach_pipeline`) is fixed;
  `cargo check --workspace` is green.
- Entity/revision ids gained `FromStr` (`EntityId`, `JobId`) for URL parsing.

## Phase 2 — koharu-rpc — done (compile-verified, no live run yet)

New crate `crates/koharu-rpc` (axum 0.8, utoipa 5, tokio-stream):

- Routes under `/api/v1`: `GET/POST /projects`, `DELETE /projects/{name}`,
  `POST /projects/{name}/open`, `GET /project`, `GET /pages`,
  `GET /pages/{id}`, `POST /pages/{id}/select`,
  `GET /pages/{id}/thumbnail` (webp), `POST /pages/import` (server-local
  paths), `POST /pages/upload` (multipart bytes; 1 GiB body limit),
  `GET /pages/{id}/export?format=` (download one rendered page),
  `GET /texts` (text-only view: original/translated pairs per page, no
  geometry), `POST /process`, `POST /process/{job}/stop`, `POST /export`,
  `GET /fonts`,
  `GET /fonts/{family}/preview` (webp), `GET/POST /preferences`,
  `GET /translation/models`. Upload makes the API fully usable from another
  machine/OS — nothing requires client and server to share a filesystem.
- `GET /api/v1/events`: SSE framing of `App::events()` — event name from the
  `Event` enum's `Display`, full tagged JSON as data, `lagged` frame on
  broadcast overflow, keep-alives on.
- `POST /project/close` closes the active project and releases loaded
  pipeline models after any running job stops. Batch clients call it after
  each project to release memory between chapters.
- Readiness: middleware returns 503 on all `/api/v1/*` until `App::is_ready()`.
- `GET /openapi.json` + `cargo run -p koharu-rpc --bin openapi` export the
  utoipa spec (16 paths, DTO schemas incl. the SSE payload types).
- `serve(app, listener)` / `serve_with_assets(app, listener, resolver)` with
  an `AssetResolver` fallback hook for the static web UI. No UI bundle is
  wired yet — the current UI still assumes Tauri (Phase 4).
- Export formats: `png`, `jpeg` (alpha flattened onto white, quality 92),
  `webp` (lossy, quality 90 — much smaller files at identical resolution),
  and `psd`. The quality constants live in `core/export.rs`.
- Import decoding is byte-based end to end (`core::import::import_payloads`
  over `PageFile{name, bytes}`); path-based imports read files first and
  delegate, so both transports share one code path.
- Error mapping (`error.rs`): domain precondition failures map to 409/404,
  everything else is a 500 with the full anyhow chain in
  `{"error": "..."}`. No CORS layer (loopback-only by design).

Divergences from the original plan, deliberate:
- `DELETE /projects/{name}` instead of `DELETE /projects` (name in the path).
- `GET /pages/{id}` fetches any page by id via `App::page_view` (desktop only
  exposes the active page).
- Foreign config payloads (`PipelineConfig`, `TypesettingConfig`,
  `ProviderConfig`, `Scope`, `Operation`, translator `Model`) are documented
  as free-form objects in the spec instead of cascading utoipa derives into
  every model crate.

## Phase 3 — headless entry — done (compile-verified, no live run yet)

- `crates/koharu/src/main.rs`: `--headless` plus `--host/--port/--config/
  --store` (clap `requires = "headless"` keeps them invalid in desktop mode).
  CEF is never touched on the headless path; the desktop branch is unchanged.
- `crates/koharu/src/headless.rs` bootstrap: `Store::configure` (all
  platforms; defaults to a `store` directory next to the executable),
  `headless.json` load, config overlays, `App::new`, bind listener, serve,
  `App::initialize`, optional warmup, `mark_ready`. The API answers 503
  while models download.
- `Pipeline::warm(stages)` → `StageRunner::warm` loads each configured
  stage's model (downloads through the existing runtime store machinery with
  progress on the event bus). Exposed as `App::warm`.
- `headless.json` (default `~/.koharu/headless.json`, `--config` overrides):
  `host`, `port`, `warmup` (`"all"` or `["detection", "ocr", "translation",
  "inpainting"]`), and optional `pipeline`/`providers`/`typesetting` sections
  that deep-merge over the live config handles in memory only — arrays
  replace wholesale, and `config.toml` is never written. CLI flags win over
  the file; default listen address is `127.0.0.1:9170`.
  `headless.example.json` at the repository root is a working example.
- `scripts/start_headless.ps1` and `.sh` launch the built binary.

Divergences from the original plan, deliberate:
- Overlay merging is a structural JSON deep-merge in the headless bootstrap
  (arrays replace) rather than reusing `koharu-config`'s private
  provider/model tag logic; sections that need exact control should be
  provided whole.
- No headless UI bundle or `open` behavior; the process serves the API until
  killed (no graceful shutdown yet).

## Remaining before calling this shippable

- Desktop smoke run (`bun run dev`): create/import/process/export through the
  UI to confirm the Phase 1 re-home did not change behavior. TS bindings were
  regenerated for the new `ExportFormat` variants (additive diff only).
- Live headless run on Windows (`scripts/start_headless.ps1`): first-run
  download, warmup, one full project cycle through HTTP + SSE, plus a remote
  client round-trip (upload from another machine via `manga.py --host`, then
  download rendered pages in jpeg/webp).
- Job listing endpoint (`GET /process/jobs`) if clients turn out to need
  polling; job state currently arrives only via SSE.

## Phase 4 — not started

- UI dual transport (HTTP/SSE in `@koharu/bridge` when not in Tauri) and
  wiring the built UI into `serve_with_assets`.
- Canvas-manipulation endpoints (the canvas DTOs already live in `core`).
- MCP layer over the same server.

## Reference implementation (old headless, deleted at `d53d3d70`)

`git show 0.61.2:koharu-rpc/src/...` — axum + utoipa + SSE + rmcp structure,
`docs/en-US/how-to/run-gui-headless-and-mcp.md` for the documented CLI shape.
Command names/signatures changed substantially since; treat as pattern
reference, not copyable code.
