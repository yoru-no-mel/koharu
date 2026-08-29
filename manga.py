#!/usr/bin/env python3
"""
manga.py — batch-translate manga images through Koharu's headless HTTP API.

Runs the full pipeline on every supported image inside an input folder:
    upload -> detection -> ocr -> translation -> inpainting -> download PNG/JPEG/WebP

Works from any machine: images are UPLOADED as multipart bytes and rendered
pages are DOWNLOADED — the server never needs the client's file paths.

Talks to the real Koharu API (koharu-rpc), mounted at the server root:
    POST /pages/upload              upload image/archive/PDF bytes
    POST /process                   start the pipeline, returns a job id
    GET  /events                    SSE stream carrying job progress events
    GET  /pages/{id}/export         download one rendered page

Translation model / provider / target language are NOT set by this script —
they come from the server's preferences (~/.koharu/headless.json overlay or
config.toml). GET /preferences is printed at startup so you can see what
will be used.

Usage:
    pip install requests

    # batch-process ./testing with defaults (output -> ./translated_testing)
    python manga.py

    # explicit input folder + output folder
    python manga.py testing --output ./out

    # recursive: each immediate subfolder becomes its own project/batch
    python manga.py chapters --recursive

    # run only some stages (default: the full chain)
    python manga.py testing --stages detection,ocr

    # smaller output files: lossy WebP at full resolution
    python manga.py testing --format webp
"""

import argparse
import json
import sys
import time
from pathlib import Path

import requests

SUPPORTED_EXTS = {".png", ".jpg", ".jpeg", ".webp"}

ALL_STAGES = ["detection", "ocr", "translation", "inpainting"]

EXPORT_EXTS = {"png": "png", "jpeg": "jpg", "webp": "webp", "psd": "psd"}

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

    # ---- Export (download) ----
    def export_pages(self, fmt, output_dir):
        """GET /pages/{id}/export per page — rendered bytes download
        straight to the local machine, so output lands on the client."""
        pages = self.list_pages()
        if not pages:
            raise RuntimeError("The project has no pages to export")
        extension = EXPORT_EXTS[fmt]
        for index, page in enumerate(pages, 1):
            label = sanitize(page.get("label") or "page")
            r = self.session.get(
                self._url(f"/pages/{page['id']}/export"),
                params={"format": fmt}, timeout=600)
            r.raise_for_status()
            path = output_dir / f"{index:04}_{label}.{extension}"
            path.write_bytes(r.content)
            print(f"  saved {path}")
        print(f"Exported {len(pages)} file(s).")


def find_images(folder: Path):
    images = [p for p in sorted(folder.iterdir())
              if p.is_file() and p.suffix.lower() in SUPPORTED_EXTS]
    if not images:
        raise SystemExit(f"No supported images ({', '.join(sorted(SUPPORTED_EXTS))}) "
                         f"found in {folder}")
    return images


def process_folder(client, args, input_dir: Path, output_dir: Path, project_name: str,
                   resume_project=None):
    """Upload one folder of images, run the pipeline, download rendered
    pages into the local output_dir."""
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
        client.import_pages(images)
        print(f"Uploaded {len(images)} page(s)")

    stages = [s.strip().lower() for s in args.stages.split(",") if s.strip()]
    for s in stages:
        if s not in ALL_STAGES:
            raise SystemExit(f"Unknown stage {s!r} — valid stages: {', '.join(ALL_STAGES)}")
    stages = None if stages == ALL_STAGES else stages
    print("Pipeline:", "full chain (detect -> ocr -> translate -> inpaint)"
          if stages is None else " -> ".join(stages))

    job_id = client.start_process(stages=stages)
    client.wait_job(job_id, timeout=args.timeout)

    print(f"Downloading rendered pages to {output_dir} ...")
    client.export_pages(args.format, output_dir)


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
    ap.add_argument("--format", choices=list(EXPORT_EXTS), default="png",
                    help="Download format (default png). jpeg/webp are lossy "
                         "at full resolution — much smaller files than png")
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

    base_url = f"http://{args.host}:{args.port}"
    client = KoharuClient(base_url)
    client.wait_ready(timeout=args.timeout)
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
                           project_name=sub.name)
    else:
        output_dir = args.output if args.output is not None \
            else Path(f"translated_{args.input.name}")
        project_name = args.project_name if args.project_name is not None \
            else args.input.name
        process_folder(client, args, args.input, output_dir,
                       project_name=project_name, resume_project=args.resume_project)

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
