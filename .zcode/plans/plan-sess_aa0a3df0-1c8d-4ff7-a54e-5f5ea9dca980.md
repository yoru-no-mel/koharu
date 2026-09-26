# Plan: release `meme` as `v0.1-win` with a full Windows zip (user creates the GitHub release)

Per your choices: full desktop zip, leave `release.yml` alone (it will show a failed run on the tag — expected and harmless), and you'll attach the asset + create the release via the GitHub web UI yourself.

## 1. Build the release executable
- `bun install` (ensures the pinned tauri-cli and UI deps are present), then `bun run build` — this runs the Next.js UI build (`bun run ui:build`) and `cargo tauri build --no-bundle` (release profile), producing `target/release/koharu.exe`. Desktop UI is embedded at compile time; headless is the same exe with `--headless`.
- Smoke test: `target/release/koharu.exe --help` (CLI responds).

## 2. Assemble the zip (outside the repo, at `D:\Dev\koharu-v0.1-win.zip`)
Contents, assembled from `target/release/`:
- `koharu.exe`
- CEF runtime required next to the exe: `libcef.dll`, `chrome_elf.dll`, `libEGL.dll`, `libGLESv2.dll`, `d3dcompiler_47.dll`, `dxcompiler.dll`, `dxil.dll`, `icudtl.dat`, `chrome_100_percent.pak`, `chrome_200_percent.pak`, `resources.pak`, `locales/`, `CREDITS.html`
- `headless.example.json` (server config overlay), `manga.py` (batch client), `scripts/start_headless.ps1`, `start-koharu.bat` (launchers)
- a short `README.txt` in the zip: quick start for desktop (`koharu.exe`) and headless (`koharu.exe --headless`, models auto-download into `store/` next to the exe), pointer to `manga.py --help` and `--list-translation-models`.

## 3. Tag and push
- `git tag v0.1-win meme` and `git push origin v0.1-win`.
- Note: this triggers `release.yml` on GitHub, which will fail (upstream-only signing secrets) — per your choice, we ignore that noise.

## 4. Hand off the release to you
- Zip at `D:\Dev\koharu-v0.1-win.zip`.
- Ready-to-paste release notes drafted for you, titled **"meme"**, summarizing the branch: headless text-only JSON API (`GET /texts`), `manga.py --text txt,epub` text-only mode, `--translation` / `--list-translation-models` model selector, new detection & OCR models (`comic-text-and-bubble-detector`, `comic-text-detector`, `pp-doclayout-v3`, `paddleocr-vl-manga`), `start-koharu.bat`, export retry hardening — plus Windows quick-start instructions and the note that `comic-text-and-bubble-detector` needs >8 GB VRAM (verified OOM on an RX 6600).
- You create the release at https://github.com/yoru-no-mel/koharu/releases/new with tag `v0.1-win` (target `meme`), title "meme", paste the notes, attach the zip, publish.

## Verification
- `koharu.exe --help` after the build; zip integrity listing (`tar -tf` or `unzip -l`); `git push` output confirming the tag.