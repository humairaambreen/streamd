# streamd

Cross-platform (macOS, Windows, Linux) pure-Rust GPU video player. Paste a direct video link—or open/drop a local or partial `.part`/`.crdownload` file—and play immediately on GPU while downloading in 24 parallel chunks. Closing the app wipes all temporary data.

## Features
- **Zero-remux hardware playback** via `libmpv` OpenGL GPU rendering (`MKV`, `MP4`, `1080p`/`4K`, `HEVC`, `10-bit`, `HDR`, `AC3`/`EAC3`/`DTS`, `ASS`/`SRT`/`PGS`).
- **24-connection parallel chunk engine** (`1 MiB` chunks, urgent seek queue, EOF prefetch, sequential `200 OK` fallback, automatic `403` retry cascade).
- **Local & partial download playback**: open or drag-and-drop any local video or `.part`/`.crdownload` file, or attach a partial file to resume downloading with its URL.
- **Subtitles**: upload local `.srt`/`.vtt`/`.ass` files (or drag-and-drop), adjust timing offset, subtitle speed (`sub-speed`), and scale, or search & load subtitles online with zero API key.
- **Self-contained releases**: GitHub Actions builds standalone bundles with `libmpv` included for macOS (`arm64` & `x64`), Windows (`x64`), and Linux (`x64`).

## Build from Source
```sh
# macOS
brew install mpv
cargo run --release

# Windows: place libmpv-2.dll next to streamd.exe
cargo run --release
```

## Controls
- `Space` / `K` — Play / Pause
- `←` / `→` / `J` / `L` — Seek ±10s
- `↑` / `↓` — Volume ±5% (up to 300%)
- `M` — Mute · `F` — Fullscreen · `C` — Subtitles · `0`–`9` — Jump 0–90%
