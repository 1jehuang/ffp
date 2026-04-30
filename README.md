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

Profiled on 20,000 recent files:

| Operation | Time |
|-----------|------|
| File loading | ~2.7s background load |
| Empty query match | ~2µs |
| Fuzzy search | ~2-3ms |
| Image thumbnail (cold) | 10-20ms |
| Image thumbnail (cached) | ~10-15µs |

### Detailed Profiling

```
profile: loaded 20000 files in 2.716906457s
profile: query <empty> -> 50 matches in 2.205µs
profile: query "rs" -> 50 matches in 3.154651ms
profile: query "main" -> 50 matches in 2.768032ms
profile: query "doc" -> 50 matches in 2.301986ms

=== Image Loading Profile ===
profile: image "38_gray.png" -> 38x38 thumb in 2.7ms
profile: image "icon-128.png" -> 599x600 thumb in 21.7ms
profile: image "jcode-vs-claude-code.png" -> 800x562 thumb in 57.4ms

=== Cached Image Access ===
profile: cold load in 135µs
profile: hot cache access in 14µs
```

## Usage

```bash
ffp              # Launch file picker
ffp --dir        # Launch directory picker
ffp --profile    # Run performance profiler
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
