# Plan: text-only JSON API + manga.py `--text txt,epub` + live test on `testing_novel`

## What exists today
- Original/translated pairs live per text layer as `TextContent { source, translation }` in the scene (`koharu-scene` `SourceText`/`Translation`, written by `koharu-pipeline/src/stages/translation.rs`). They're only exposed inside the heavy full-page view `GET /pages/{id}` — no lightweight text-only endpoint exists.
- `manga.py` always downloads rendered images via `GET /pages/{id}/export` after the pipeline finishes.

## Part 1 — Text-only JSON endpoint (`GET /texts`)

**koharu-app (`crates/koharu-app/src/core/project.rs`, `app.rs`)**
1. New DTOs in `core/project.rs` (same derive set as `Page`/`TextContent`):
   ```rust
   pub struct PageText {                 // one per page, project page order
       pub id: EntityId,
       pub label: String,
       pub segments: Vec<TextSegment>,   // scene layer order (OCR/reading order)
   }
   pub struct TextSegment {
       pub id: EntityId,                 // TextContent entity id
       pub source: Option<String>,       // original text
       pub translation: Option<String>,  // translated text
   }
   ```
   Lean by design — no geometry/regions/typography. Pages with no text layers still appear with empty `segments` so labels/chapter structure survives.
2. `ProjectStore` (`core/project.rs`): `pub(crate) fn texts(snapshot, page) -> Result<Vec<TextSegment>>` — walks text layers the same way `layer_view` (~line 1029) does (`SceneTextLayout` → `snapshot.text_layer(layer)` → `content.source()` / `content.translation()`), collecting only the two strings.
3. `App` (`core/app.rs`, near `page_view` line 484): `pub async fn texts(&self) -> Result<Vec<PageText>>` — iterates the project's pages in order and maps each through the collector.

**koharu-rpc (`crates/koharu-rpc/src`)**
4. `routes.rs`: handler `get_texts` — `#[utoipa::path(get, path = "/api/v1/texts", body = [PageText])]`, thin `Json(app.texts().await?)` like `get_pages`.
5. `lib.rs` `router()` (~line 38): `.route("/texts", get(routes::get_texts))` (root-mounted like all routes) + add to `ApiDoc` paths (~line 148).

## Part 2 — `manga.py --text txt,epub`

6. `ap.add_argument("--text", default=None)` accepting a comma-separated list with values validated against `{txt, epub}` — e.g. `--text txt`, `--text epub`, or `--text txt,epub` to write **both outputs from a single pipeline run** (no second run needed). When set, `process_folder` skips `export_pages` entirely (`--format` ignored, note printed if passed) and instead:
7. New client method `fetch_texts()` → `GET /texts` (fetched once, reused for every requested format).
8. One output file **per format** per project into `output_dir` (so `--recursive` yields one `.txt` + one `.epub` per subfolder):
   - **txt**: `<output_dir>/<project_name>.txt` — per page a `== <label> ==` header, then translation segments joined by newlines (translated text only; the JSON endpoint still carries originals). Server order preserved.
   - **epub**: `<output_dir>/<project_name>.epub` via stdlib `zipfile` (no new deps): stored `mimetype`, `META-INF/container.xml`, `OEBPS/content.opf` + `nav.xhtml` (EPUB 3, title = project name), one XHTML chapter per page (label as heading, each segment a `<p>`). Empty pages → empty chapters, page count preserved.
9. Update module docstring/usage examples.

## Part 3 — Live test on `testing_novel` (explicitly requested, output in both txt and epub)

`testing_novel/` contains `horizontal/` (5 jpgs) and `vertical/` (5 jpgs) — each becomes its own project via `--recursive`.

1. Ensure a headless server is reachable: probe `http://127.0.0.1:9170/project`; if down, start `cargo run -p koharu -- --headless` in the background (debug) and wait for readiness (warmup may take a while).
2. For each detection model — `comic-text-detector` and `koharu-layout-rfdetr-seg-2xl` (two separate passes since server preferences are global):
   ```
   python manga.py testing_novel --recursive --text txt,epub --fresh \
       --detection <model> --output test_out/<model>
   ```
   Each pass runs both the horizontal and vertical folders and writes one `.txt` **and** one `.epub` per project.
3. Verify via `curl GET /texts` on all four projects (2 models × horizontal/vertical) that JSON contains `source` + `translation` pairs, correct page order, and sensible segment counts for both horizontal and vertical pages; confirm the `.txt` and `.epub` files exist and contain the translated text with no mojibake (open the epub's XHTML chapters to check).
4. Compare the two detection models on this novel-style content (segments found, reading order, coverage) and report the differences.

## Verification summary
- `cargo check -p koharu-rpc` (debug, once) covers app-core + rpc changes.
- `python manga.py --help` smoke test.
- Live runs per Part 3 on both detection models, horizontal + vertical, with both txt and epub outputs produced.