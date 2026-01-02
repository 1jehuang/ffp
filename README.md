# ffp - Fast File Picker

A fuzzy file picker for the terminal with image preview support using the Kitty graphics protocol.

## Features

- Fuzzy file search with rapidfuzz
- Image preview (PNG, JPEG, GIF, WebP) via Kitty graphics protocol
- Text file preview
- Files sorted by modification time (most recent first)
- LRU thumbnail cache for fast image switching

## Performance

Profiled on ~5300 files:

| Operation | Time |
|-----------|------|
| File loading | ~90ms |
| Empty query match | ~1µs |
| Fuzzy search | ~500-600µs |
| Image thumbnail (cold) | 10-20ms |
| Image thumbnail (cached) | <1µs |

### Detailed Profiling

```
profile: loaded 5302 files in 89.89986ms
profile: query <empty> -> 50 matches in 1.115µs
profile: query "rs" -> 25 matches in 581.573µs
profile: query "main" -> 50 matches in 566.038µs
profile: query "doc" -> 50 matches in 523.604µs

=== Image Loading Profile ===
profile: image "chrome-context-menu.png" -> 296x300 thumb in 14.5ms
profile: image "istockphoto-1024x1024.jpg" -> 300x300 thumb in 15.0ms
profile: image "small-icon-128.png" -> 128x128 thumb in 0.17ms

=== Cached Image Access ===
profile: cold load in 11.6ms
profile: hot cache access in 632ns
```

## Usage

```bash
ffp              # Launch file picker
ffp --profile    # Run performance profiler
```

## Keybindings

| Key | Action |
|-----|--------|
| `Enter` | Open selected file |
| `Esc` | Cancel |
| `↑/↓` | Navigate |
| `Ctrl+N/J` | Next item |
| `Ctrl+P/K` | Previous item |
| `Ctrl+U` | Clear query |
| `Ctrl+W` | Delete word |

## Requirements

- Kitty terminal (for image preview)
- `fd` command for file discovery

## Build

```bash
cargo build --release
```
