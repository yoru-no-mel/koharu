#!/usr/bin/env python3
"""
koharu_batch_render.py

Same pipeline as koharu_batch_translate.py, but saves ONLY the rendered
translated page images (no .txt text files). Always runs the full chain:
    import -> detect -> segment -> bubble-segment -> ocr -> translate
            -> inpaint -> render -> export image(s)

Based on: https://koharu.rs/reference/http-api/

Requires Koharu to already be running (GUI or `koharu --headless`) with the
HTTP API reachable, e.g.:

    koharu --headless --port 4000

Usage:
    pip install requests

    # minimal — uses defaults for host/port/model/steps, output becomes en_<input>
    python koharu_batch_render.py test2

    # override anything as needed
    python koharu_batch_render.py test2 \
        --output ./translated_images --host 192.168.0.2 --port 4000 \
        --local-model qwen3.5-9b-uncensored --target-language en

    # or use a remote provider instead of a local model:
    python koharu_batch_render.py test2 --provider openai-compatible --model openai/gpt-5.6-luna

    # recursive: duo/chapter_1, duo/chapter_2, ... -> each processed as its
    # own project, rendered images saved to duo/en/chapter_1, duo/en/chapter_2, ...
    python koharu_batch_render.py duo --recursive \
        --provider openai-compatible --model openai/gpt-5.6-luna
"""

import argparse
import io
import json
import mimetypes
import sys
import time
import zipfile
from pathlib import Path

import requests

SUPPORTED_EXTS = {".png", ".jpg", ".jpeg", ".webp"}


class KoharuClient:
    def __init__(self, base_url: str):
        self.base = base_url.rstrip("/")
        self.session = requests.Session()

    def _url(self, path: str) -> str:
        return f"{self.base}{path}"

    def wait_ready(self, timeout=120):
        """Poll /meta until the app has finished bootstrapping (no more 503)."""
        print("Waiting for Koharu API to be ready...")
        start = time.time()
        while time.time() - start < timeout:
            try:
                r = self.session.get(self._url("/meta"))
                if r.status_code == 200:
                    print("Koharu is ready:", r.json())
                    return
            except requests.exceptions.ConnectionError:
                pass
            time.sleep(1)
        raise RuntimeError("Timed out waiting for Koharu API to become ready")

    # ---- Projects ----
    def create_project(self, name: str) -> str:
        r = self.session.post(self._url("/projects"), json={"name": name})
        r.raise_for_status()
        data = r.json()
        return data["id"]

    def open_project(self, project_id: str):
        r = self.session.put(self._url("/projects/current"), json={"id": project_id})
        r.raise_for_status()

    # ---- Pages ----
    def import_pages(self, image_paths):
        """POST /pages with multipart image files. Returns response JSON."""
        files = []
        for p in image_paths:
            files.append(("files", (p.name, open(p, "rb"), _mime_for(p))))
        try:
            r = self.session.post(self._url("/pages"), files=files)
            r.raise_for_status()
            return r.json()
        finally:
            for _, (_, fh, _) in files:
                fh.close()

    # ---- LLM control ----
    def get_llm_state(self):
        r = self.session.get(self._url("/llm/current"))
        r.raise_for_status()
        return r.json()

    def ensure_llm_ready(self, kind, provider_id, model_id):
        """Check GET /llm/current first. If it's already 'ready' with a
        matching target, reuse it and skip PUT /llm/current entirely
        (loading an already-loaded model appears to 422). Otherwise, error
        out to the console rather than attempting a load."""
        state = self.get_llm_state()
        status = state.get("status")
        target = state.get("target") or {}

        if status == "ready":
            same_kind = target.get("kind") == kind
            same_model = target.get("modelId") == model_id
            same_provider = (kind != "provider") or (target.get("providerId") == provider_id)
            if same_kind and same_model and same_provider:
                print(f"LLM already ready: {target}")
                return state
            print(
                f"ERROR: LLM is ready but loaded with a different target.\n"
                f"  currently loaded: {target}\n"
                f"  requested:        kind={kind}, modelId={model_id}, providerId={provider_id}\n"
                f"Load/switch the model manually (GUI or PUT /llm/current) and re-run.",
                file=sys.stderr,
            )
            sys.exit(1)

        print(
            f"ERROR: LLM is not ready (status={status}, error={state.get('error')}).\n"
            f"Load a model first (GUI, or PUT /llm/current), then re-run this script.",
            file=sys.stderr,
        )
        sys.exit(1)

    # ---- Engines (to sanity-check pipeline step ids) ----
    def list_engines(self):
        r = self.session.get(self._url("/engines"))
        r.raise_for_status()
        return r.json()

    # ---- Pipelines ----
    def run_pipeline(self, steps, pages=None, target_language=None):
        body = {"steps": steps}
        if pages:
            body["pages"] = pages
        if target_language:
            body["targetLanguage"] = target_language
        r = self.session.post(self._url("/pipelines"), json=body)
        r.raise_for_status()
        return r.json()["operationId"]

    # ---- Operations ----
    def get_operation(self, operation_id, debug=True):
        r = self.session.get(self._url("/operations"))
        r.raise_for_status()
        data = r.json()

        def _search(container):
            if isinstance(container, list):
                for item in container:
                    if isinstance(item, dict) and item.get("id") == operation_id:
                        return item
            elif isinstance(container, dict):
                # dict keyed by operation id -> object
                if operation_id in container and isinstance(container[operation_id], dict):
                    obj = dict(container[operation_id])
                    obj.setdefault("id", operation_id)
                    return obj
            return None

        if isinstance(data, dict):
            # Try common nested containers first (docs mention jobs + downloads registries)
            for key in ("jobs", "operations", "downloads"):
                if key in data:
                    found = _search(data[key])
                    if found:
                        return found
            found = _search(data)
            if found:
                return found
        else:
            found = _search(data)
            if found:
                return found

        # Nothing matched — dump the raw shape once so we can fix the parsing.
        if debug:
            print(f"DEBUG: could not find operation {operation_id} in /operations response:\n"
                  f"{json.dumps(data, indent=2)[:2000]}")
        return None

    def wait_operation(self, operation_id, timeout=3600, poll_every=3):
        print(f"Waiting for pipeline operation {operation_id} to finish...")
        start = time.time()
        printed_debug = False
        last_print = 0
        last_status_blob = None
        while time.time() - start < timeout:
            op = self.get_operation(operation_id, debug=not printed_debug)
            elapsed = time.time() - start
            if op is None:
                printed_debug = True
                if elapsed - last_print >= 5:
                    print(f"  [{elapsed:5.0f}s] waiting for operation to appear...")
                    last_print = elapsed
                time.sleep(poll_every)
                continue

            status = op.get("status")
            extra = {k: v for k, v in op.items() if k not in ("id", "kind", "status")}
            status_blob = (status, json.dumps(extra, sort_keys=True))

            # Print whenever status/extra fields change, and at least every 5s
            # so it's clear the script is still alive on long batches.
            if status_blob != last_status_blob or elapsed - last_print >= 5:
                extra_str = f" {extra}" if extra else ""
                print(f"  [{elapsed:5.0f}s] status={status}{extra_str}")
                last_print = elapsed
                last_status_blob = status_blob

            if status in ("finished", "completed", "success"):
                print(f"Pipeline finished in {elapsed:.0f}s.")
                return op
            if status == "completed_with_errors":
                print(f"WARNING: Pipeline finished with errors in {elapsed:.0f}s: {op.get('error')}")
                print("  Continuing to export whatever rendered successfully — check /events "
                      "or rerun with --dump-scene to see which page(s)/step(s) were affected.")
                return op
            if status in ("failed", "error", "cancelled"):
                raise RuntimeError(f"Pipeline operation failed: {op}")
            time.sleep(poll_every)
        raise RuntimeError("Timed out waiting for pipeline operation")

    # ---- Scene (only used for optional --dump-scene troubleshooting) ----
    def get_scene(self):
        r = self.session.get(self._url("/scene.json"))
        r.raise_for_status()
        return r.json()

    # ---- Export ----
    def export(self, fmt="rendered", pages=None):
        """POST /projects/current/export. format is one of khr, psd, rendered,
        inpainted. Multi-file results come back as application/zip; a
        single-page/-file result comes back as the raw file bytes with an
        appropriate Content-Type."""
        body = {"format": fmt}
        if pages:
            body["pages"] = pages
        r = self.session.post(self._url("/projects/current/export"), json=body)
        r.raise_for_status()
        return r.content, r.headers.get("Content-Type", "")


def _mime_for(path: Path) -> str:
    ext = path.suffix.lower()
    return {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".webp": "image/webp",
    }.get(ext, "application/octet-stream")


def find_images(folder: Path):
    images = [p for p in sorted(folder.iterdir()) if p.suffix.lower() in SUPPORTED_EXTS]
    if not images:
        raise SystemExit(f"No supported images ({', '.join(SUPPORTED_EXTS)}) found in {folder}")
    return images


def save_export_result(content: bytes, content_type: str, images, out_dir: Path):
    """Save the bytes from KoharuClient.export() into out_dir, matching
    output files back to original input filenames where possible."""
    saved = []
    if "zip" in content_type.lower():
        zf = zipfile.ZipFile(io.BytesIO(content))
        names = sorted(zf.namelist())
        stem_map = {img.stem.lower(): img for img in images}

        # Pass 1: direct filename-stem matches take priority over fallback
        # order, regardless of zip entry iteration order.
        assigned = {}  # name -> Path
        used_stems = set()
        for name in names:
            target = stem_map.get(Path(name).stem.lower())
            if target is not None:
                assigned[name] = target
                used_stems.add(target.stem.lower())

        # Pass 2: remaining entries get the next unused image, in filename order.
        remaining_images = iter(img for img in sorted(images, key=lambda p: p.name)
                                 if img.stem.lower() not in used_stems)
        for name in names:
            if name not in assigned:
                assigned[name] = next(remaining_images, None)

        for name in names:
            data = zf.read(name)
            entry_path = Path(name)
            entry_ext = entry_path.suffix or ".png"
            target = assigned.get(name)
            out_name = (target.stem if target else entry_path.stem) + entry_ext
            out_path = out_dir / out_name
            out_path.write_bytes(data)
            saved.append(out_path)
    else:
        ext = mimetypes.guess_extension(content_type.split(";")[0].strip()) or ".png"
        stem = images[0].stem if len(images) == 1 else "rendered"
        out_path = out_dir / (stem + ext)
        out_path.write_bytes(content)
        saved.append(out_path)
    return saved


def process_folder(client, args, input_dir: Path, output_dir: Path, project_name: str,
                    resume_project=None, resume_operation=None):
    """Run the full import -> pipeline -> export-image flow for one folder
    of images. Used directly for a single folder, and once per subfolder
    when --recursive is set."""
    output_dir.mkdir(parents=True, exist_ok=True)
    images = find_images(input_dir)
    print(f"Found {len(images)} image(s) in {input_dir}")

    if resume_project:
        # Reattach to an existing project instead of creating + re-importing.
        client.open_project(resume_project)
        print(f"Reopened project {resume_project}")
    else:
        # 1. Project
        project_id = client.create_project(project_name)
        client.open_project(project_id)
        print(f"Opened project {project_id}")

        # 2. Import pages (filename-sorted natural order per docs)
        client.import_pages(images)
        print("Imported pages")

    if resume_operation:
        op_id = resume_operation
        print(f"Reattaching to existing pipeline operation {op_id}")
    else:
        # 3. Run pipeline: detect -> segment -> bubble-segment -> ocr -> translate
        #    -> inpaint -> render. Always the full chain — image-only output has
        #    nothing to produce without inpaint/render.
        steps = [s.strip() for s in args.steps.split(",") if s.strip()]
        # Inpainters need both a SegmentMask and a BubbleMask. Some detectors
        # (e.g. comic-text-detector) already produce a SegmentMask alongside
        # TextBoxes; others (pp-doclayout-v3, comic-text-bubble-detector,
        # anime-text) only produce TextBoxes. No detector produces a BubbleMask
        # on its own — that always needs the dedicated bubble-segmenter engine.
        # Insert whatever's missing right after detection and before OCR.
        mask_producing_detectors = {"comic-text-detector"}
        extra_steps = []
        if not any(s in mask_producing_detectors for s in steps):
            extra_steps.append(args.segmenter)
        extra_steps.append(args.bubble_segmenter)
        insert_at = 1 if steps else 0
        steps[insert_at:insert_at] = extra_steps
        steps += [args.inpainter, args.renderer]
        print(f"Pipeline steps: {' -> '.join(steps)}")
        op_id = client.run_pipeline(steps=steps, target_language=args.target_language)

    client.wait_operation(op_id, timeout=args.timeout)

    # 4. Optional debugging dump (not needed for normal image-only use)
    if args.dump_scene:
        scene_json = client.get_scene()
        dump_path = output_dir / "_scene_dump.json"
        dump_path.write_text(json.dumps(scene_json, indent=2, ensure_ascii=False), encoding="utf-8")
        print(f"Dumped raw scene to {dump_path}")

    # 5. Export the rendered (translated) page images and save them — this is
    #    the only output this script produces.
    print("Exporting rendered page images...")
    content, content_type = client.export(fmt="rendered")
    saved = save_export_result(content, content_type, images, output_dir)
    for p in saved:
        print(f"Saved rendered image {p}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("input", type=Path, help="Folder of images to process (or, with "
                                              "--recursive, a folder of per-chapter subfolders)")
    ap.add_argument("--output", type=Path, default=None,
                     help="Output folder for the rendered images. Default: en_<input folder "
                          "name> normally, or <input>/en with --recursive (each chapter's "
                          "subfolder name is reused underneath).")
    ap.add_argument("--recursive", action="store_true", default=False,
                     help="Treat each immediate subfolder of INPUT as its own chapter/batch: "
                          "e.g. duo/chapter_1, duo/chapter_2 -> duo/en/chapter_1, "
                          "duo/en/chapter_2. Each subfolder gets its own Koharu project "
                          "and pipeline run, processed one at a time.")
    ap.add_argument("--host", default="192.168.0.2", help="Koharu API host (default 192.168.0.2)")
    ap.add_argument("--port", type=int, default=4000, help="Koharu API port (default 4000)")
    ap.add_argument("--project-name", default=None,
                     help="Name for the Koharu project to create (default: input folder name; "
                          "ignored with --recursive, which uses each subfolder's name)")
    ap.add_argument("--target-language", default="en", help="Target language code, e.g. en")

    ap.add_argument("--provider", help="Provider id: openai, gemini, claude, deepseek, deepl, "
                                        "google-translate, caiyun, openai-compatible "
                                        "(overrides --local-model if set)")
    ap.add_argument("--local-model", default="qwen3.5-9b-uncensored",
                     help="Local model id to load (default: qwen3.5-9b-uncensored; "
                          "ignored if --provider is set)")
    ap.add_argument("--model", help="Model id for --provider (required if --provider is set)")

    ap.add_argument("--steps", default="pp-doclayout-v3,paddle-ocr-vl-1.6,llm",
                     help="Comma-separated detect/ocr/translate engine ids to run, in order, "
                          "before inpaint+render are appended automatically "
                          "(default: pp-doclayout-v3,paddle-ocr-vl-1.6,llm). Run with "
                          "--list-engines to see what's registered on your build.")
    ap.add_argument("--inpainter", default="lama-manga",
                     help="Inpainter engine id (default: lama-manga; see --list-engines for "
                          "other options e.g. flux2-klein, aot-inpainting)")
    ap.add_argument("--segmenter", default="comic-text-detector-seg",
                     help="Segmenter engine id used to produce the SegmentMask that "
                          "inpainters require (default: comic-text-detector-seg). Only "
                          "inserted if the configured detector doesn't already produce a "
                          "mask on its own.")
    ap.add_argument("--bubble-segmenter", default="speech-bubble-segmentation",
                     help="Bubble-segmenter engine id used to produce the BubbleMask that "
                          "inpainters require (default: speech-bubble-segmentation). Always "
                          "inserted since no detector engine produces a BubbleMask on its own.")
    ap.add_argument("--renderer", default="koharu-renderer",
                     help="Renderer engine id (default: koharu-renderer)")
    ap.add_argument("--list-engines", action="store_true",
                     help="Print registered engines per pipeline stage and exit")
    ap.add_argument("--dump-scene", action="store_true", default=False,
                     help="Also dump raw scene.json to <output>/_scene_dump.json for "
                          "troubleshooting (default: off; not needed for normal use since "
                          "this script only saves images)")
    ap.add_argument("--timeout", type=int, default=10800,
                     help="Max seconds to wait for the pipeline to finish (default 10800 = 3h). "
                          "Large batches on a local model can take a long time — this only "
                          "affects how long THIS script waits; the job keeps running on the "
                          "Koharu side regardless.")
    ap.add_argument("--resume-project", default=None,
                     help="Skip project creation + page import: reopen this existing project id "
                          "(from a previous run's 'Opened project <id>' line). Not supported "
                          "together with --recursive.")
    ap.add_argument("--resume-operation", default=None,
                     help="Skip re-running the pipeline: just wait on this existing operation id "
                          "(from a previous run's 'Waiting for pipeline operation <id>' line), "
                          "then export the image. Requires --resume-project too. Not supported "
                          "together with --recursive.")
    args = ap.parse_args()

    if args.resume_operation and not args.resume_project:
        ap.error("--resume-operation requires --resume-project")

    if args.provider and not args.model:
        ap.error("--model is required when using --provider")

    if args.recursive and (args.resume_project or args.resume_operation):
        ap.error("--resume-project/--resume-operation aren't supported together with --recursive "
                 "(resume the specific chapter's subfolder directly instead, without --recursive)")

    base_url = f"http://{args.host}:{args.port}/api/v1"
    client = KoharuClient(base_url)
    client.wait_ready()

    if args.list_engines:
        print(json.dumps(client.list_engines(), indent=2))
        return

    # LLM is shared across the whole run (single/recursive) — check once up front.
    if args.provider:
        client.ensure_llm_ready(kind="provider", provider_id=args.provider, model_id=args.model)
    else:
        client.ensure_llm_ready(kind="local", provider_id=None, model_id=args.local_model)

    if args.recursive:
        subdirs = sorted([p for p in args.input.iterdir() if p.is_dir()], key=lambda p: p.name)
        if not subdirs:
            raise SystemExit(f"--recursive was set but {args.input} has no subfolders")
        output_root = args.output if args.output is not None else (args.input / "en")
        print(f"Found {len(subdirs)} subfolder(s) under {args.input}; output -> {output_root}")

        for idx, sub in enumerate(subdirs, 1):
            try:
                find_images(sub)
            except SystemExit:
                print(f"[{idx}/{len(subdirs)}] Skipping {sub} (no supported images)")
                continue
            sub_output = output_root / sub.name
            print(f"\n=== [{idx}/{len(subdirs)}] {sub} -> {sub_output} ===")
            process_folder(client, args, sub, sub_output, project_name=sub.name)
    else:
        output_dir = args.output if args.output is not None else Path(f"en_{args.input.name}")
        project_name = args.project_name if args.project_name is not None else args.input.name
        process_folder(client, args, args.input, output_dir, project_name=project_name,
                        resume_project=args.resume_project, resume_operation=args.resume_operation)

    print("Done.")


if __name__ == "__main__":
    try:
        main()
    except requests.exceptions.HTTPError as e:
        print(f"HTTP error: {e} — response body: {e.response.text if e.response else ''}", file=sys.stderr)
        sys.exit(1)
    except RuntimeError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)