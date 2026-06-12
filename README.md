# ffp - Fast File Picker

A fuzzy file picker for the terminal with image and video preview support using the Kitty graphics protocol.
ffp starts with your recent files, so you can select recently updated or opened files without searching through your entire computer.

<img width="2877" height="1762" alt="image" src="https://github.com/user-attachments/assets/d3937bbd-80fb-4eec-8ee5-4b3c984f08ad" />
<img width="2877" height="1762" alt="image" src="https://github.com/user-attachments/assets/8fabe1f0-131d-495f-8153-eccf2038eeb8" />

## Features

- Fuzzy file search with rapidfuzz
- Image preview (PNG, JPEG, GIF, WebP) via Kitty graphics protocol
- Text file preview
- Files sorted by combined recency: ffp open history, desktop recent files, access time, then modification time
- Recent discovery defaults to files interacted with within the last 9 days
- Preview reads avoid updating access time where Linux permits `O_NOATIME`
- Press `Ctrl+Z` to toggle between recent files and all files
- LRU thumbnail cache for fast image switching

## Recency model

Recent mode merges multiple sources and keeps the UI responsive:

1. `ffp` history for files selected through `ffp`, stored at `~/.local/state/ffp/recent.tsv`.
2. Desktop recent files from `~/.local/share/recently-used.xbel`.
3. Filesystem access time (`atime`) and modification time (`mtime`).

The recent pool is capped to the 20,000 most recent entries after filtering generated/dependency-heavy directories such as `target`, `.rustup`, Cargo registries, Go module cache, virtualenvs, and `__pycache__`.

Linux usually uses `relatime`, so filesystem `atime` may update at most daily for repeated reads. The `ffp` history makes files opened through `ffp` precise even when `atime` is not updated.

## Performance

File metadata is gathered on a parallel stat-worker pool, fuzzy scoring fans
out across CPU cores for large pools, and the startup full-tree scan also
fills the Ctrl+Z all-files pool so the toggle is instant.

Profiled on 20,000 recent files (and 600,000 all-files entries):

| Operation | Time |
|-----------|------|
| Recent pool load | ~0.9s background load |
| Empty query match | ~2µs |
| Fuzzy search (20k recent pool) | ~0.5-3ms |
| Fuzzy search (600k all pool) | ~20-30ms |
| Image thumbnail (cold) | 15-25ms |
| Image thumbnail (cached) | ~1-2µs |

### Detailed Profiling

```
profile: loaded 20000 files in 914.291389ms
profile: query <empty> -> 50 matches in 1.482µs
profile: query "rs" -> 50 matches in 1.16141ms
profile: query "main" -> 50 matches in 2.627592ms
profile: query "doc" -> 50 matches in 2.743233ms

=== Incremental Typing Profile ===
profile: incremental query "m" -> 50 matches in 498µs
profile: incremental query "ma" -> 50 matches in 1.13ms
profile: incremental query "mai" -> 50 matches in 1.34ms
profile: incremental query "main" -> 50 matches in 542µs

=== Cached Image Access ===
profile: cold load in 17.9ms
profile: hot cache access in 1.6µs
```

## Usage

```bash
ffp              # Launch file picker
ffp --dir        # Launch directory picker
ffp --profile    # Run performance profiler
ffp --bench-responsiveness  # Benchmark input/search/preview responsiveness
FFP_BENCH_SYNTHETIC=600000 ffp --bench-responsiveness  # Benchmark at all-files scale
ffp --trace-responsive      # Log slow input/render events to ~/.local/state/ffp/responsiveness.log
```

By default, `ffp` searches files only. Use `--dir` to start in directory mode, or press `Ctrl+P` in the app to toggle between files and directories.

## Keybindings

| Key | Action |
|-----|--------|
| `Enter` | Open selected file |
| `Esc` | Cancel |
| `↑/↓` | Navigate |
| `Ctrl+N/J` | Next item |
| `Ctrl+K` | Previous item |
| `Ctrl+P` | Toggle between files and directories |
| `Ctrl+Z` | Toggle between recent-only and all-files search |
| `Ctrl+U` | Clear query |
| `Ctrl+W` | Delete word |

## Requirements

- Kitty terminal (for image preview)
- `fd` command for file discovery

## Build

```bash
cargo build --release
```
