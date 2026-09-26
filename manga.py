#!/usr/bin/env python3
"""
manga.py — batch-translate manga images through Koharu's headless HTTP API.

Runs the full pipeline on every supported image inside an input folder:
    upload -> detection -> ocr -> translation -> inpainting -> download PNG/JPEG/WebP

Works from any machine: images are UPLOADED as multipart bytes and rendered
pages are DOWNLOADED — the server never needs the client's file paths.

With --text, rendered images are skipped entirely and the translated text is
saved instead (as .txt and/or .epub), fetched from the text-only JSON API:
    GET  /texts                     original + translated text per page

Talks to the real Koharu API (koharu-rpc), mounted at the server root:
    POST /pages/upload              upload image/archive/PDF bytes
    POST /process                   start the pipeline, returns a job id
    GET  /events                    SSE stream carrying job progress events
    GET  /pages/{id}/export         download one rendered page
    GET  /texts                     original/translated text per page
    POST /project/close             release the project and loaded models

Models: --detection and --ocr pick from the fixed choices; --translation
selects any model the server offers (see --list-translation-models). The
target language and generation settings always come from the server's
preferences (~/.koharu/headless.json overlay or config.toml). GET
/preferences is printed at startup so you can see what will be used.

Usage:
    pip install requests

    # batch-process ./testing with defaults (output -> ./translated_testing)
    python manga.py

    # explicit input folder + output folder
    python manga.py testing --output ./out

    # recursive: each immediate subfolder becomes its own project/batch
    python manga.py chapters --recursive

    # text-only: skip image export, save the translation as txt AND epub
    python manga.py testing_novel --recursive --text txt,epub

    # pick models: detection, ocr, and any translation model on the server
    python manga.py testing --detection comic-text-detector --translation gemma-4-uncensored

    # see which translation models the server can use
    python manga.py --list-translation-models

    # run only some stages (default: the full chain)
    python manga.py testing --stages detection,ocr

    # smaller output files: lossy WebP at full resolution
    python manga.py testing --format webp
"""

import argparse
import html
import json
import sys
import time
import uuid
import zipfile
from datetime import datetime, timezone
from pathlib import Path

import requests

SUPPORTED_EXTS = {".png", ".jpg", ".jpeg", ".webp"}

ALL_STAGES = ["detection", "ocr", "translation", "inpainting"]

EXPORT_EXTS = {"png": "png", "jpeg": "jpg", "webp": "webp", "psd": "psd"}

TEXT_FORMATS = {"txt", "epub"}

# One multipart request stays well under the server's 1 GiB body limit;
# batches are appended in order, so page ordering is preserved.
UPLOAD_BATCH_BYTES = 192 * 1024 * 1024


def sanitize(label: str) -> str:
    """Mirrors the server's export file naming: strip extension, replace
    characters Windows forbids, fall back to 'page'."""
    name = label.strip().rstrip(". ")
    name = name.rsplit(".", 1)[0] if "." in name else name
    name = "".join("_" if c in '<>:"/\\|?*' else c for c in name)
    return name or "page"


class KoharuClient:
    def __init__(self, base_url: str):
        self.base = base_url.rstrip("/")
        self.session = requests.Session()

    def _url(self, path: str) -> str:
        return f"{self.base}{path}"

    # ---- Readiness ----
    def wait_ready(self, timeout=3600):
        """Every API route answers 503 until the app finished bootstrapping
        (model warmup included). Poll a cheap route until it turns 200."""
        print("Waiting for Koharu API to be ready (downloads/warmup may take a while)...")
        start = time.time()
        while time.time() - start < timeout:
            try:
                r = self.session.get(self._url("/project"), timeout=10)
                if r.status_code == 200:
                    print(f"Koharu is ready after {time.time() - start:.0f}s.")
                    return
            except requests.exceptions.ConnectionError:
                pass
            time.sleep(2)
        raise RuntimeError("Timed out waiting for Koharu API to become ready")

    # ---- Preferences (informational) ----
    def show_preferences(self):
        try:
            r = self.session.get(self._url("/preferences"), timeout=10)
            r.raise_for_status()
            pipeline = r.json().get("pipeline", {})
            print("Active pipeline models:",
                  json.dumps({k: pipeline.get(k) for k in
                              ("detection", "ocr", "translation", "inpainting")},
                             ensure_ascii=False))
        except Exception as e:
            print(f"Could not read preferences ({e}) — continuing anyway.")

    # ---- Preferences (model selection) ----
    def set_pipeline_models(self, detection=None, ocr=None):
        """POST /preferences with the current values except for the requested
        detection/ocr model. Model configs (thresholds) come from the
        server's stored processor profiles."""
        r = self.session.get(self._url("/preferences"), timeout=30)
        r.raise_for_status()
        prefs = r.json()
        pipeline = prefs["pipeline"]
        if detection:
            pipeline["detection"] = {"model": detection}
        if ocr:
            pipeline["ocr"] = {"model": ocr}
        r = self.session.post(self._url("/preferences"),
                              json={"pipeline": pipeline,
                                    "providers": prefs["providers"],
                                    "typesetting": prefs["typesetting"]},
                              timeout=30)
        r.raise_for_status()
        selected = r.json()["pipeline"]
        print(f"Selected detection={selected['detection']['model']} "
              f"ocr={selected['ocr']['model']}")

    # ---- Translation model selection ----
    def list_translation_models(self):
        """GET /translation/models — every model the server can translate
        with, as `provider/model` strings ready for --translation."""
        r = self.session.get(self._url("/translation/models"), timeout=30)
        r.raise_for_status()
        for model in r.json():
            provider = model["provider"]
            name = model.get("model") or model["name"]
            quants = ",".join(q["id"] for q in model.get("quantizations") or [])
            caps = [c for c, on in (("vision", model.get("vision")),
                                    ("reasoning", model.get("reasoning"))) if on]
            print(f"{provider}/{name}  "
                  f"({'quantizations: ' + quants + '  ' if quants else ''}"
                  f"{' '.join(caps)})")

    def set_translation_model(self, spec):
        """Select a translation model via POST /preferences. SPEC is either
        `model` (matched against model ids and display names) or
        `provider/model` to disambiguate. The server's available models come
        from GET /translation/models — quantization defaults to the first
        listed for the model unless the current selection already picks a
        valid one."""
        models = self.session.get(self._url("/translation/models"), timeout=30).json()
        wanted_provider, wanted_model = (None, spec)
        if "/" in spec:
            wanted_provider, _, wanted_model = spec.partition("/")
        matches = [m for m in models
                   if (wanted_provider is None or m["provider"] == wanted_provider)
                   and (m.get("model") == wanted_model or m["name"] == wanted_model)]
        if not matches:
            available = "\n".join(f"  {m['provider']}/{m.get('model') or m['name']}"
                                  for m in models)
            raise SystemExit(f"No translation model matches {spec!r}. "
                             f"Available:\n{available}\n(list with --list-translation-models)")
        if len(matches) > 1:
            raise SystemExit(
                f"{spec!r} is ambiguous — matches: "
                + ", ".join(f"{m['provider']}/{m.get('model') or m['name']}"
                            for m in matches)
                + " (use provider/model to disambiguate)")
        model = matches[0]

        r = self.session.get(self._url("/preferences"), timeout=30)
        r.raise_for_status()
        prefs = r.json()
        current = prefs["pipeline"]["translation"]["model"]
        quantization = None
        quant_ids = [q["id"] for q in model.get("quantizations") or []]
        if quant_ids:
            if current.get("provider") == model["provider"] \
                    and current.get("model") == model.get("model") \
                    and current.get("quantization") in quant_ids:
                quantization = current["quantization"]
            else:
                quantization = quant_ids[0]
        prefs["pipeline"]["translation"]["model"] = {
            "provider": model["provider"],
            "model": model.get("model"),
            "quantization": quantization,
            "vision": bool(model.get("vision")),
            "reasoning": bool(model.get("reasoning")),
        }
        r = self.session.post(self._url("/preferences"),
                              json={"pipeline": prefs["pipeline"],
                                    "providers": prefs["providers"],
                                    "typesetting": prefs["typesetting"]},
                              timeout=30)
        r.raise_for_status()
        selected = r.json()["pipeline"]["translation"]["model"]
        quant = f" quantization={selected['quantization']}" if selected["quantization"] else ""
        print(f"Selected translation={selected['provider']}/"
              f"{selected['model'] or selected.get('name', '')}{quant}")

    # ---- Projects ----
    def create_project(self, name: str):
        """POST /projects creates AND opens the project."""
        r = self.session.post(self._url("/projects"), json={"name": name}, timeout=30)
        if r.status_code != 201:
            raise RuntimeError(
                f"Creating project {name!r} failed (HTTP {r.status_code}): {r.text}\n"
                f"If it already exists, rerun with --fresh (delete + recreate) or "
                f"--resume-project {name} (reuse imported pages).")
        return r.json()

    def open_project(self, name: str):
        r = self.session.post(self._url(f"/projects/{name}/open"), timeout=30)
        r.raise_for_status()
        return r.json()

    def close_project(self):
        """POST /project/close releases the active project and loaded models."""
        r = self.session.post(self._url("/project/close"), timeout=600)
        r.raise_for_status()

    def delete_project(self, name: str):
        r = self.session.delete(self._url(f"/projects/{name}"), timeout=30)
        r.raise_for_status()

    # ---- Pages ----
    def import_pages(self, image_paths):
        """POST /pages/upload — multipart upload of file bytes. The file
        names decide page order (natural sort) and container format; the
        server expands CBZ/ZIP/RAR/PDF just like a local import would."""
        batches = []
        current, size = [], 0
        for path in image_paths:
            file_size = path.stat().st_size
            if current and size + file_size > UPLOAD_BATCH_BYTES:
                batches.append(current)
                current, size = [], 0
            current.append(path)
            size += file_size
        if current:
            batches.append(current)

        for index, batch in enumerate(batches, 1):
            handles = [path.open("rb") for path in batch]
            try:
                payload = [("files", (path.name, handle))
                           for path, handle in zip(batch, handles)]
                r = self.session.post(self._url("/pages/upload"),
                                      files=payload, timeout=1800)
            finally:
                for handle in handles:
                    handle.close()
            r.raise_for_status()
            print(f"  uploaded batch {index}/{len(batches)} ({len(batch)} file(s))")

    def list_pages(self):
        r = self.session.get(self._url("/pages"), timeout=30)
        r.raise_for_status()
        return r.json()

    # ---- Pipeline ----
    def start_process(self, stages=None):
        """POST /process — scope=project, operation=full (or the requested
        stages). Returns the job id (uuid). Progress arrives on /events."""
        operation = {"operation": "full"} if stages is None else \
            {"operation": "stages", "stages": stages}
        body = {"scope": {"scope": "project"}, "operation": operation}
        r = self.session.post(self._url("/process"), json=body, timeout=30)
        r.raise_for_status()
        return r.json()  # bare uuid string

    def stop_job(self, job_id):
        self.session.post(self._url(f"/process/{job_id}/stop"), timeout=30)

    # ---- Events (SSE) ----
    def stream_events(self):
        r = self.session.get(self._url("/events"), stream=True, timeout=(10, None))
        r.raise_for_status()
        event_name = None
        for raw in r.iter_lines(decode_unicode=True):
            if raw is None:
                continue
            line = raw.strip()
            if not line:
                event_name = None
                continue
            if line.startswith("event:"):
                event_name = line.split(":", 1)[1].strip()
            elif line.startswith("data:") and event_name:
                yield event_name, line.split(":", 1)[1].strip()

    def wait_job(self, job_id, timeout=10800, poll_every=0):
        """Consume the SSE stream until the job with `job_id` reaches a
        terminal state (finished / failed / stopped)."""
        print(f"Waiting for job {job_id} ...")
        start = time.time()
        last_line = None
        for event_name, data in self.stream_events():
            if event_name == "lagged":
                print(f"  WARNING: event stream lagged, {data} events skipped")
                continue
            if event_name != "job":
                continue
            try:
                job = json.loads(data)
            except json.JSONDecodeError:
                continue
            if str(job.get("id", "")).lower() != str(job_id).lower():
                continue
            state = job.get("state")
            stage = job.get("stage")
            model = job.get("model")
            line = (f"[{time.time() - start:5.0f}s] "
                    f"{job.get('completed', 0)}/{job.get('total', 0)}"
                    f" stage={stage or '-'} model={model or '-'}")
            if line != last_line:
                print(line)
                last_line = line
            if state == "finished":
                print(f"Job finished in {time.time() - start:.0f}s.")
                return
            if state == "stopped":
                raise RuntimeError("Job was stopped before finishing")
            if state == "failed":
                raise RuntimeError(f"Job failed: {job.get('error')}")
        raise RuntimeError("Event stream ended without a terminal job state")

    # ---- Text-only export ----
    def fetch_texts(self):
        """GET /texts — per page, the original + translated text of every
        text layer in scene (reading) order. No images are involved, so this
        works even when the rasterizer can't run on the server's GPU."""
        r = self.session.get(self._url("/texts"), timeout=60)
        r.raise_for_status()
        return r.json()

    # ---- Export (download) ----
    def export_one(self, page_id, fmt, attempts=6, backoff=10):
        """GET one rendered page, retrying transient 5xx failures. The ML
        models keep ~6.5 GB of the GPU's 8 GB VRAM loaded after a pipeline
        run, so the rasterizer's wgpu device creation can fail while the
        GPU is under memory pressure; it usually clears within a minute."""
        for attempt in range(1, attempts + 1):
            r = self.session.get(
                self._url(f"/pages/{page_id}/export"),
                params={"format": fmt}, timeout=600)
            if r.status_code < 500:
                r.raise_for_status()
                return r.content
            if attempt < attempts:
                print(f"  export attempt {attempt} failed (HTTP {r.status_code}: "
                      f"{r.text[:120]}...) — retrying in {backoff}s")
                time.sleep(backoff)
        r.raise_for_status()
        return r.content

    def export_pages(self, fmt, output_dir):
        """GET /pages/{id}/export per page — rendered bytes download
        straight to the local machine, so output lands on the client."""
        pages = self.list_pages()
        if not pages:
            raise RuntimeError("The project has no pages to export")
        extension = EXPORT_EXTS[fmt]
        for index, page in enumerate(pages, 1):
            label = sanitize(page.get("label") or "page")
            content = self.export_one(page["id"], fmt)
            path = output_dir / f"{index:04}_{label}.{extension}"
            path.write_bytes(content)
            print(f"  saved {path}")
        print(f"Exported {len(pages)} file(s).")


def find_images(folder: Path):
    images = [p for p in sorted(folder.iterdir())
              if p.is_file() and p.suffix.lower() in SUPPORTED_EXTS]
    if not images:
        raise SystemExit(f"No supported images ({', '.join(sorted(SUPPORTED_EXTS))}) "
                         f"found in {folder}")
    return images


def segment_text(segment: dict) -> str:
    """The translated text of one text layer; pages processed without the
    translation stage fall back to the OCR'd original so nothing is lost."""
    return (segment.get("translation") or segment.get("source") or "").strip()


def write_txt(pages, path: Path):
    """One plain-text file: per page a `== label ==` header followed by the
    translated segments in server order, separated by blank lines."""
    chunks = []
    for page in pages:
        chunks.append(f"== {page.get('label') or 'page'} ==")
        for segment in page.get("segments") or []:
            text = segment_text(segment)
            if text:
                chunks.append(text)
        chunks.append("")
    path.write_text("\n".join(chunks).rstrip() + "\n", encoding="utf-8")


EPUB_CHAPTER = """<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>{title}</title></head>
  <body>
    <h2>{title}</h2>
{paragraphs}
  </body>
</html>
"""

EPUB_NAV = """<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
  <head><title>Contents</title></head>
  <body>
    <nav epub:type="toc"><h1>Contents</h1><ol>
{items}
    </ol></nav>
  </body>
</html>
"""

EPUB_OPF = """<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:{book_id}</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:language>und</dc:language>
    <meta property="dcterms:modified">{modified}</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
{manifest_items}
  </manifest>
  <spine>
    <itemref idref="nav"/>
{spine_items}
  </spine>
</package>
"""

EPUB_CONTAINER = """<?xml version="1.0" encoding="utf-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"""


def write_epub(pages, path: Path, title: str):
    """EPUB 3 via stdlib zipfile: one XHTML chapter per page, each translated
    segment as a <p>. Empty pages stay in the book as empty chapters so the
    page count matches the source."""
    chapters = []
    for index, page in enumerate(pages, 1):
        label = page.get("label") or f"page {index}"
        paragraphs = "\n".join(
            f"    <p>{html.escape(text)}</p>"
            for segment in (page.get("segments") or [])
            if (text := segment_text(segment)))
        chapters.append((f"page{index:04}.xhtml", label,
                         EPUB_CHAPTER.format(title=html.escape(label),
                                             paragraphs=paragraphs)))

    with zipfile.ZipFile(path, "w") as book:
        # mimetype must be the first entry, stored uncompressed (EPUB spec)
        book.writestr(zipfile.ZipInfo("mimetype"),
                      "application/epub+zip", zipfile.ZIP_STORED)
        book.writestr("META-INF/container.xml", EPUB_CONTAINER, zipfile.ZIP_DEFLATED)
        book.writestr("OEBPS/nav.xhtml",
                      EPUB_NAV.format(items="\n".join(
                          f'      <li><a href="{name}">{html.escape(label)}</a></li>'
                          for name, label, _ in chapters)), zipfile.ZIP_DEFLATED)
        book.writestr("OEBPS/content.opf", EPUB_OPF.format(
            book_id=uuid.uuid4(), title=html.escape(title),
            modified=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            manifest_items="\n".join(
                f'    <item id="c{i}" href="{name}" media-type="application/xhtml+xml"/>'
                for i, (name, _, _) in enumerate(chapters, 1)),
            spine_items="\n".join(
                f'    <itemref idref="c{i}"/>' for i, _ in enumerate(chapters, 1))),
            zipfile.ZIP_DEFLATED)
        for name, _, content in chapters:
            book.writestr(f"OEBPS/{name}", content, zipfile.ZIP_DEFLATED)


def process_folder(client, args, input_dir: Path, output_dir: Path, project_name: str,
                   resume_project=None, text_formats=None):
    """Upload one folder of images, run the pipeline, then either download
    rendered pages or save the translated text into the local output_dir."""
    output_dir.mkdir(parents=True, exist_ok=True)
    images = find_images(input_dir)
    print(f"Found {len(images)} image(s) in {input_dir}")

    if resume_project:
        client.open_project(resume_project)
        print(f"Reopened project {resume_project!r} (pages not re-imported)")
    else:
        if args.fresh:
            try:
                client.delete_project(project_name)
                print(f"Deleted existing project {project_name!r}")
            except requests.exceptions.HTTPError:
                pass  # didn't exist — nothing to delete
        client.create_project(project_name)
        print(f"Created project {project_name!r}")
        try:
            client.import_pages(images)
        except BaseException:
            client.close_project()
            raise
        print(f"Uploaded {len(images)} page(s)")

    try:
        stages = [s.strip().lower() for s in args.stages.split(",") if s.strip()]
        for s in stages:
            if s not in ALL_STAGES:
                raise SystemExit(f"Unknown stage {s!r} — valid stages: {', '.join(ALL_STAGES)}")
        stages = None if stages == ALL_STAGES else stages
        print("Pipeline:", "full chain (detect -> ocr -> translate -> inpaint)"
              if stages is None else " -> ".join(stages))

        job_id = client.start_process(stages=stages)
        client.wait_job(job_id, timeout=args.timeout)

        if text_formats:
            print("Fetching translated text ...")
            pages = client.fetch_texts()
            if not pages:
                raise RuntimeError("The project has no pages to export")
            stem = sanitize(project_name)
            for fmt in text_formats:
                path = output_dir / f"{stem}.{fmt}"
                if fmt == "txt":
                    write_txt(pages, path)
                else:
                    write_epub(pages, path, title=project_name)
                print(f"  saved {path}")
        else:
            print(f"Downloading rendered pages to {output_dir} ...")
            client.export_pages(args.format or "webp", output_dir)
    finally:
        client.close_project()
        print(f"Closed project {resume_project or project_name!r} and released loaded models")


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("input", nargs="?", default="testing", type=Path,
                    help="Folder of images to process (default: ./testing; with "
                         "--recursive, each immediate subfolder is its own batch)")
    ap.add_argument("--output", type=Path, default=None,
                    help="Output folder for rendered images. Default: "
                         "translated_<input folder name> normally, or "
                         "<input>/translated with --recursive")
    ap.add_argument("--recursive", action="store_true", default=False,
                    help="Treat each immediate subfolder of INPUT as its own "
                         "chapter/batch, one project and pipeline run each")
    ap.add_argument("--host", default="192.168.0.2",
                    help="Koharu API host (default 192.168.0.2)")
    ap.add_argument("--port", type=int, default=9170,
                    help="Koharu API port (default 9170)")
    ap.add_argument("--project-name", default=None,
                    help="Koharu project name (default: input folder name; "
                         "ignored with --recursive, which uses each subfolder name)")
    ap.add_argument("--stages", default=",".join(ALL_STAGES),
                    help="Comma-separated subset of stages to run: "
                         f"{','.join(ALL_STAGES)} (default: all). Export always "
                         "renders whatever state the pages are in")
    ap.add_argument("--detection", default=None,
                    choices=["koharu-layout-rfdetr-seg-2xl", "comic-text-and-bubble-detector",
                             "comic-text-detector", "pp-doclayout-v3"],
                    help="Detection model to select on the server before running "
                         "(default: leave the server's current selection)")
    ap.add_argument("--ocr", default=None,
                    choices=["paddleocr-vl-1.6", "paddleocr-vl-manga", "manga-ocr",
                             "baberu-ocr", "hayai-ocr"],
                    help="OCR model to select on the server before running "
                         "(default: leave the server's current selection)")
    ap.add_argument("--translation", default=None, metavar="SPEC",
                    help="Translation model to select on the server before "
                         "running, as `model` or `provider/model` (default: "
                         "leave the server's current selection). See "
                         "--list-translation-models")
    ap.add_argument("--list-translation-models", action="store_true", default=False,
                    help="Print every translation model the server offers "
                         "(as `provider/model` for --translation) and exit")
    ap.add_argument("--format", choices=list(EXPORT_EXTS), default=None,
                    help="Download format (default webp). jpeg/webp are lossy "
                         "at full resolution — much smaller files than png. "
                         "Ignored when --text is set")
    ap.add_argument("--text", default=None, metavar="txt,epub",
                    help="Comma-separated text output formats: txt and/or epub "
                         "(e.g. --text txt,epub writes both). Skips image "
                         "export entirely — the translated text of every page "
                         "is saved as <project>.txt/.epub instead")
    ap.add_argument("--fresh", action="store_true", default=False,
                    help="Delete an existing project of the same name first, "
                         "instead of failing")
    ap.add_argument("--resume-project", default=None,
                    help="Skip project creation + page upload: reopen this "
                         "existing project name. Not supported with --recursive")
    ap.add_argument("--timeout", type=int, default=10800,
                    help="Max seconds to wait for a pipeline run (default 3h)")
    args = ap.parse_args()

    if args.recursive and args.resume_project:
        ap.error("--resume-project isn't supported together with --recursive")

    text_formats = None
    if args.text is not None:
        text_formats = [f.strip().lower() for f in args.text.split(",") if f.strip()]
        unknown = [f for f in text_formats if f not in TEXT_FORMATS]
        if unknown:
            ap.error(f"Unknown --text format(s) {', '.join(unknown)} — "
                     f"valid formats: {', '.join(sorted(TEXT_FORMATS))}")
        if not text_formats:
            ap.error("--text needs at least one format: txt or epub")
        if args.format is not None:
            print(f"Note: --format is ignored with --text (text-only mode).")

    base_url = f"http://{args.host}:{args.port}"
    client = KoharuClient(base_url)
    client.wait_ready(timeout=args.timeout)
    if args.list_translation_models:
        client.list_translation_models()
        return
    if args.detection or args.ocr:
        client.set_pipeline_models(detection=args.detection, ocr=args.ocr)
    if args.translation:
        client.set_translation_model(args.translation)
    client.show_preferences()

    if args.recursive:
        subdirs = sorted((p for p in args.input.iterdir() if p.is_dir()),
                         key=lambda p: p.name)
        if not subdirs:
            raise SystemExit(f"--recursive was set but {args.input} has no subfolders")
        output_root = args.output if args.output is not None \
            else (args.input / "translated")
        print(f"Found {len(subdirs)} subfolder(s) under {args.input}; "
              f"output -> {output_root}")
        for idx, sub in enumerate(subdirs, 1):
            try:
                find_images(sub)
            except SystemExit:
                print(f"[{idx}/{len(subdirs)}] Skipping {sub} (no supported images)")
                continue
            print(f"\n=== [{idx}/{len(subdirs)}] {sub} -> {output_root / sub.name} ===")
            process_folder(client, args, sub, output_root / sub.name,
                           project_name=sub.name, text_formats=text_formats)
    else:
        output_dir = args.output if args.output is not None \
            else Path(f"translated_{args.input.name}")
        project_name = args.project_name if args.project_name is not None \
            else args.input.name
        process_folder(client, args, args.input, output_dir,
                       project_name=project_name, resume_project=args.resume_project,
                       text_formats=text_formats)

    print("Done.")


if __name__ == "__main__":
    try:
        main()
    except requests.exceptions.HTTPError as e:
        print(f"HTTP error: {e} — response body: "
              f"{e.response.text if e.response is not None else ''}", file=sys.stderr)
        sys.exit(1)
    except RuntimeError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("Interrupted.", file=sys.stderr)
        sys.exit(130)
