use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use image::GenericImageView;
use rapidfuzz::fuzz::ratio;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use std::{
    cmp::Ordering,
    collections::HashMap,
    env,
    fs,
    io::{self, BufRead, BufReader, Write},
    os::unix::fs::FileTypeExt,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

const MATCH_LIMIT: usize = 50;
const FILE_BATCH_SIZE: usize = 256;
const IMAGE_CACHE_SIZE: usize = 10;
const THUMBNAIL_MAX_WIDTH: u32 = 800;
const THUMBNAIL_MAX_HEIGHT: u32 = 600;
const VIDEO_PREVIEW_FRAMES: usize = 75;
const VIDEO_FRAME_INTERVAL_MS: u64 = 66;

#[derive(Clone)]
struct CachedImage {
    data: Vec<u8>,  // PNG-encoded thumbnail
    width: u32,
    height: u32,
}

struct ImageCache {
    cache: HashMap<String, CachedImage>,
    order: Vec<String>,  // LRU order
}

impl ImageCache {
    fn new() -> Self {
        Self {
            cache: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn get(&mut self, path: &str) -> Option<&CachedImage> {
        if self.cache.contains_key(path) {
            // Move to end (most recently used)
            self.order.retain(|p| p != path);
            self.order.push(path.to_string());
            self.cache.get(path)
        } else {
            None
        }
    }

    fn insert(&mut self, path: String, image: CachedImage) {
        // Evict oldest if at capacity
        while self.order.len() >= IMAGE_CACHE_SIZE {
            if let Some(old) = self.order.first().cloned() {
                self.cache.remove(&old);
                self.order.remove(0);
            }
        }
        self.order.push(path.clone());
        self.cache.insert(path, image);
    }
}

struct FileEntry {
    path: String,
    name: String,
    name_lower: String,
    dir: String,
    mtime: SystemTime,
}

#[derive(Clone)]
struct MatchEntry {
    index: usize,
    score: f64,
}

enum PreviewContent {
    Text(Vec<Line<'static>>),
    Image(CachedImage),
    Video(Vec<CachedImage>),
    VideoLoading,
    None,
}

struct App {
    query: String,
    files: Arc<Mutex<Vec<FileEntry>>>,
    matches: Vec<MatchEntry>,
    matches_by_time: Vec<MatchEntry>,
    selected: usize,
    active_column: usize, // 0 = ranked, 1 = recent
    loading: Arc<Mutex<bool>>,
    last_query: String,
    last_file_count: usize,
    preview_path: String,
    preview_content: PreviewContent,
    image_cache: ImageCache,
    video_frame: usize,
    video_frame_time: Instant,
    video_loader: Option<Arc<Mutex<Option<Vec<CachedImage>>>>>,
    video_loader_path: String,
    syntax_set: SyntaxSet,
    theme: syntect::highlighting::Theme,
}

impl App {
    fn new() -> Self {
        let files = Arc::new(Mutex::new(Vec::new()));
        let loading = Arc::new(Mutex::new(true));

        // Spawn file loader thread
        let files_clone = Arc::clone(&files);
        let loading_clone = Arc::clone(&loading);
        thread::spawn(move || {
            load_files(files_clone, loading_clone);
        });

        let syntax_set = SyntaxSet::load_defaults_newlines();
        let mut ts = ThemeSet::load_defaults();
        let theme = ts.themes.remove("base16-ocean.dark").unwrap();

        Self {
            query: String::new(),
            files,
            matches: Vec::new(),
            matches_by_time: Vec::new(),
            selected: 0,
            active_column: 0,
            loading,
            last_query: String::new(),
            last_file_count: 0,
            preview_path: String::new(),
            preview_content: PreviewContent::None,
            image_cache: ImageCache::new(),
            video_frame: 0,
            video_frame_time: Instant::now(),
            video_loader: None,
            video_loader_path: String::new(),
            syntax_set,
            theme,
        }
    }

    fn update_preview(&mut self) {
        let current_path = self.selected_file().unwrap_or_default();
        if current_path == self.preview_path {
            // Check if async video loader finished
            let loaded_frames = self.video_loader.as_ref().and_then(|loader| {
                loader.lock().unwrap().take()
            });
            if let Some(frames) = loaded_frames {
                self.video_loader = None;
                if !frames.is_empty() {
                    self.video_frame = 0;
                    self.video_frame_time = Instant::now();
                    self.preview_content = PreviewContent::Video(frames);
                } else {
                    self.preview_content = PreviewContent::Text(vec![
                        Line::from(Span::styled("[Cannot load video]", Style::default().fg(Color::DarkGray)))
                    ]);
                }
            }
            return;
        }
        self.preview_path = current_path.clone();

        // Cancel any in-flight video loader for a different path
        if self.video_loader_path != current_path {
            self.video_loader = None;
            self.video_loader_path.clear();
        }

        if current_path.is_empty() {
            self.preview_content = PreviewContent::None;
            return;
        }

        if is_video_file(&current_path) {
            let result = Arc::new(Mutex::new(None));
            let result_clone = Arc::clone(&result);
            let path = current_path.clone();
            thread::spawn(move || {
                let frames = load_video_frames(&path).unwrap_or_default();
                *result_clone.lock().unwrap() = Some(frames);
            });
            self.video_loader = Some(result);
            self.video_loader_path = current_path.clone();
            self.preview_content = PreviewContent::VideoLoading;
        } else if is_image_file(&current_path) {
            // Check cache first
            if let Some(cached) = self.image_cache.get(&current_path) {
                self.preview_content = PreviewContent::Image(cached.clone());
                return;
            }
            if let Some(img) = load_thumbnail(&current_path) {
                self.image_cache.insert(current_path.clone(), img.clone());
                self.preview_content = PreviewContent::Image(img);
            } else {
                self.preview_content = PreviewContent::Text(vec![
                    Line::from(Span::styled("[Cannot load image]", Style::default().fg(Color::DarkGray)))
                ]);
            }
        } else {
            let lines = read_preview(&current_path, 40);
            // Error/info messages (single line starting with '[') stay plain
            if lines.len() == 1 && lines[0].starts_with('[') {
                self.preview_content = PreviewContent::Text(vec![
                    Line::from(Span::styled(lines[0].clone(), Style::default().fg(Color::DarkGray)))
                ]);
            } else {
                self.preview_content = PreviewContent::Text(
                    highlight_preview(&lines, &current_path, &self.syntax_set, &self.theme)
                );
            }
        }
    }

    fn update_matches(&mut self) {
        let files = self.files.lock().unwrap();
        let files_len = files.len();
        let query_changed = self.query != self.last_query;
        let files_changed = files_len != self.last_file_count;

        if !query_changed && !files_changed {
            return;
        }

        self.matches = fuzzy_match(&self.query, &files, MATCH_LIMIT);

        // Time-sorted column: same matches, sorted by mtime then score
        self.matches_by_time = self.matches.clone();
        self.matches_by_time.sort_by(|a, b| {
            files[b.index]
                .mtime
                .cmp(&files[a.index].mtime)
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(Ordering::Equal)
                })
        });

        self.last_query = self.query.clone();
        self.last_file_count = files_len;
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn selected_file(&self) -> Option<String> {
        let files = self.files.lock().unwrap();
        let matches = if self.active_column == 0 {
            &self.matches
        } else {
            &self.matches_by_time
        };
        matches
            .get(self.selected)
            .and_then(|m| files.get(m.index))
            .map(|entry| entry.path.clone())
    }

    fn active_matches(&self) -> &[MatchEntry] {
        if self.active_column == 0 {
            &self.matches
        } else {
            &self.matches_by_time
        }
    }

    fn is_loading(&self) -> bool {
        *self.loading.lock().unwrap()
    }
}

fn load_files(files: Arc<Mutex<Vec<FileEntry>>>, loading: Arc<Mutex<bool>>) {
    let child = Command::new("fd")
        .args([
            ".",
            &dirs::home_dir().unwrap().to_string_lossy().to_string(),
            "/tmp",
            "-t", "f",
            "--changed-within", "2d",
            "-H",
            "-E", ".git",
            "-E", "node_modules",
            "-E", ".cache",
            "-E", ".npm",
        ])
        .stdout(Stdio::piped())
        .spawn();

    if let Ok(mut child) = child {
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut buffer = Vec::new();

            for read_result in reader.split(b'\n') {
                let line = match read_result {
                    Ok(line) => line,
                    Err(_) => break,
                };

                if line.is_empty() {
                    continue;
                }

                let path = String::from_utf8_lossy(&line).to_string();
                buffer.push(FileEntry::new(path));

                if buffer.len() >= FILE_BATCH_SIZE {
                    files.lock().unwrap().extend(buffer.drain(..));
                }
            }

            if !buffer.is_empty() {
                files.lock().unwrap().extend(buffer);
            }
        }
        let _ = child.wait();
    }

    // Sort by mtime descending (most recent first)
    files.lock().unwrap().sort_by(|a, b| b.mtime.cmp(&a.mtime));

    *loading.lock().unwrap() = false;
}

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

fn dirname(path: &str) -> &str {
    Path::new(path)
        .parent()
        .and_then(|s| s.to_str())
        .unwrap_or("")
}

impl FileEntry {
    fn new(path: String) -> Self {
        let name = basename(&path).to_string();
        let dir = dirname(&path).to_string();
        let name_lower = name.to_lowercase();
        let mtime = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        Self {
            path,
            name,
            name_lower,
            dir,
            mtime,
        }
    }
}

fn read_preview(path: &str, max_lines: usize) -> Vec<String> {
    if path.is_empty() {
        return vec!["No file selected".to_string()];
    }

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec!["[Cannot read file]".to_string()],
    };

    let reader = BufReader::new(file);
    let mut lines = Vec::new();
    let mut bytes_read = 0;
    const MAX_BYTES: usize = 16384; // Allow larger previews

    for line_result in reader.lines().take(max_lines) {
        match line_result {
            Ok(mut line) => {
                bytes_read += line.len();
                if bytes_read > MAX_BYTES {
                    break;
                }
                // Check for binary content (null bytes)
                if line.contains('\0') {
                    return vec!["[Binary file]".to_string()];
                }
                // Truncate long lines (char-aware to avoid splitting multi-byte chars)
                if line.chars().count() > 60 {
                    let truncated: String = line.chars().take(57).collect();
                    line = format!("{}...", truncated);
                }
                // Replace tabs
                line = line.replace('\t', "  ");
                lines.push(line);
            }
            Err(_) => break,
        }
    }

    if lines.is_empty() {
        vec!["[Empty file]".to_string()]
    } else {
        lines
    }
}

fn is_image_file(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico")
}

fn is_video_file(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    matches!(ext.as_str(), "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v")
}

fn get_video_duration(path: &str) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-show_entries", "format=duration",
            "-of", "csv=p=0",
            path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout);
    s.trim().parse::<f64>().ok()
}

fn load_video_frames(path: &str) -> Option<Vec<CachedImage>> {
    let duration = get_video_duration(path).unwrap_or(5.0);
    // Spread frames evenly across the whole video
    let fps = (VIDEO_PREVIEW_FRAMES as f64 / duration).max(0.5);
    let scale_filter = format!(
        "fps={:.4},scale={}:{}:force_original_aspect_ratio=decrease",
        fps, THUMBNAIL_MAX_WIDTH, THUMBNAIL_MAX_HEIGHT
    );
    let output = Command::new("ffmpeg")
        .args([
            "-i", path,
            "-vf", &scale_filter,
            "-frames:v", &VIDEO_PREVIEW_FRAMES.to_string(),
            "-f", "image2pipe",
            "-vcodec", "png",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }

    // Split concatenated PNG stream by PNG magic bytes
    let png_magic: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let data = &output.stdout;
    let mut frames = Vec::new();
    let mut start = 0;

    for i in 1..data.len().saturating_sub(7) {
        if data[i..i + 8] == png_magic {
            let frame_data = data[start..i].to_vec();
            if let Ok(img) = image::load_from_memory(&frame_data) {
                let (w, h) = img.dimensions();
                frames.push(CachedImage { data: frame_data, width: w, height: h });
            }
            start = i;
        }
    }
    // Last frame
    if start < data.len() {
        let frame_data = data[start..].to_vec();
        if let Ok(img) = image::load_from_memory(&frame_data) {
            let (w, h) = img.dimensions();
            frames.push(CachedImage { data: frame_data, width: w, height: h });
        }
    }

    if frames.is_empty() { None } else { Some(frames) }
}

fn time_ago(mtime: SystemTime) -> String {
    match mtime.elapsed() {
        Ok(elapsed) => {
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("{}s", secs)
            } else if secs < 3600 {
                format!("{}m", secs / 60)
            } else if secs < 86400 {
                format!("{}h", secs / 3600)
            } else {
                format!("{}d", secs / 86400)
            }
        }
        Err(_) => "0s".to_string(),
    }
}

fn highlight_preview(
    lines: &[String],
    path: &str,
    ss: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
) -> Vec<Line<'static>> {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let syntax = ss
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut h = HighlightLines::new(syntax, theme);

    lines
        .iter()
        .map(|line| {
            match h.highlight_line(line, ss) {
                Ok(ranges) => {
                    let spans: Vec<Span<'static>> = ranges
                        .into_iter()
                        .map(|(style, text)| {
                            let fg = Color::Rgb(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b,
                            );
                            Span::styled(text.to_string(), Style::default().fg(fg))
                        })
                        .collect();
                    Line::from(spans)
                }
                Err(_) => {
                    Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(Color::Gray),
                    ))
                }
            }
        })
        .collect()
}

fn load_thumbnail(path: &str) -> Option<CachedImage> {
    let img = image::open(path).ok()?;

    // Calculate thumbnail size maintaining aspect ratio
    let (orig_w, orig_h) = img.dimensions();
    let scale = f64::min(
        THUMBNAIL_MAX_WIDTH as f64 / orig_w as f64,
        THUMBNAIL_MAX_HEIGHT as f64 / orig_h as f64,
    );
    let scale = scale.min(1.0); // Don't upscale

    let new_w = (orig_w as f64 * scale) as u32;
    let new_h = (orig_h as f64 * scale) as u32;

    // Use fast resize filter for speed
    let thumbnail = img.resize_exact(new_w, new_h, image::imageops::FilterType::Triangle);

    // Encode to PNG for kitty protocol
    let mut png_data = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_data);
    thumbnail.write_to(&mut cursor, image::ImageFormat::Png).ok()?;

    Some(CachedImage {
        data: png_data,
        width: new_w,
        height: new_h,
    })
}

/// Render image using kitty graphics protocol
fn render_kitty_image(img: &CachedImage, area: Rect) -> io::Result<()> {
    let mut stdout = io::stdout();

    // Save cursor position, delete previous image
    write!(stdout, "\x1b7\x1b_Ga=d,d=a\x1b\\")?;

    // Calculate display cells that preserve aspect ratio.
    // Terminal cells are ~2x taller than wide, so we correct for that.
    let cell_ratio = 2.0_f64; // cell_height / cell_width in pixels
    let img_ratio = (img.width as f64 / img.height as f64) * cell_ratio;
    let area_ratio = area.width as f64 / area.height as f64;

    let (cols, rows) = if img_ratio > area_ratio {
        // Image is wider than area — fit to width
        let c = area.width as u32;
        let r = ((c as f64) / img_ratio).max(1.0) as u32;
        (c, r.min(area.height as u32))
    } else {
        // Image is taller than area — fit to height
        let r = area.height as u32;
        let c = ((r as f64) * img_ratio).max(1.0) as u32;
        (c.min(area.width as u32), r)
    };

    // Center the image in the available area
    let offset_x = (area.width as u32).saturating_sub(cols) / 2;
    let offset_y = (area.height as u32).saturating_sub(rows) / 2;

    // Move cursor to centered position (1-indexed)
    write!(
        stdout,
        "\x1b[{};{}H",
        area.y as u32 + 1 + offset_y,
        area.x as u32 + 1 + offset_x
    )?;

    // Encode image data as base64
    let b64_data = BASE64.encode(&img.data);

    // Send image in chunks (kitty protocol limit is 4096 bytes per chunk)
    let chunk_size = 4096;
    let chunks: Vec<&str> = b64_data
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;
        if i == 0 {
            write!(
                stdout,
                "\x1b_Ga=T,f=100,c={},r={},m={};{}\x1b\\",
                cols,
                rows,
                if is_last { 0 } else { 1 },
                chunk
            )?;
        } else {
            write!(
                stdout,
                "\x1b_Gm={};{}\x1b\\",
                if is_last { 0 } else { 1 },
                chunk
            )?;
        }
    }

    // Restore cursor position
    write!(stdout, "\x1b8")?;
    stdout.flush()?;
    Ok(())
}

/// Clear any kitty graphics at the given area
fn clear_kitty_image() -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1b_Ga=d,d=a\x1b\\")?;
    stdout.flush()?;
    Ok(())
}

fn get_icon(filename: &str) -> (&'static str, Color) {
    let ext = Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        // Documents
        "pdf" => ("\u{f1c1}", Color::Red),
        "doc" | "docx" => ("\u{f1c2}", Color::Blue),
        "xls" | "xlsx" => ("\u{f1c3}", Color::Green),
        "ppt" | "pptx" => ("\u{f1c4}", Color::Rgb(255, 120, 0)),
        "md" => ("\u{e73e}", Color::White),
        "txt" | "log" => ("\u{f15c}", Color::White),
        "tex" | "latex" => ("\u{f15c}", Color::Green),
        // Programming languages
        "rs" => ("\u{e7a8}", Color::Rgb(222, 165, 132)),
        "py" => ("\u{e73c}", Color::Rgb(55, 118, 171)),
        "js" | "mjs" | "cjs" => ("\u{e74e}", Color::Yellow),
        "ts" | "mts" => ("\u{e628}", Color::Rgb(49, 120, 198)),
        "tsx" | "jsx" => ("\u{e7ba}", Color::Cyan),
        "cpp" | "cc" | "cxx" | "hpp" => ("\u{e61d}", Color::Rgb(0, 89, 156)),
        "c" | "h" => ("\u{e61e}", Color::Rgb(85, 85, 205)),
        "go" => ("\u{e626}", Color::Cyan),
        "java" => ("\u{e738}", Color::Rgb(176, 114, 25)),
        "kt" | "kts" => ("\u{e634}", Color::Rgb(150, 75, 210)),
        "swift" => ("\u{e755}", Color::Rgb(255, 106, 51)),
        "rb" => ("\u{e739}", Color::Red),
        "php" => ("\u{e73d}", Color::Rgb(119, 123, 180)),
        "lua" => ("\u{e620}", Color::Blue),
        "zig" => ("\u{f0e7}", Color::Rgb(247, 164, 29)),
        "nim" => ("\u{f005}", Color::Yellow),
        "hs" | "lhs" => ("\u{e777}", Color::Rgb(94, 80, 134)),
        "ml" | "mli" | "ocaml" => ("\u{e63a}", Color::Rgb(238, 122, 0)),
        "ex" | "exs" => ("\u{e62d}", Color::Rgb(110, 74, 128)),
        "erl" => ("\u{e7b1}", Color::Red),
        "r" | "R" => ("\u{f25d}", Color::Rgb(39, 108, 194)),
        "scala" => ("\u{e737}", Color::Red),
        "dart" => ("\u{e798}", Color::Cyan),
        "v" | "sv" | "svh" => ("\u{f085}", Color::Rgb(0, 140, 140)),
        // Shell & config
        "sh" | "bash" | "zsh" | "fish" => ("\u{e795}", Color::Green),
        "nix" => ("\u{f313}", Color::Rgb(126, 186, 228)),
        // Web
        "html" | "htm" => ("\u{e736}", Color::Rgb(228, 77, 38)),
        "css" | "scss" | "sass" | "less" => ("\u{e749}", Color::Rgb(86, 61, 124)),
        "vue" => ("\u{e6a0}", Color::Green),
        "svelte" => ("\u{e697}", Color::Rgb(255, 62, 0)),
        // Data & config
        "json" | "jsonc" => ("\u{e60b}", Color::Yellow),
        "yaml" | "yml" => ("\u{f481}", Color::Rgb(203, 56, 55)),
        "toml" => ("\u{e6b2}", Color::Rgb(156, 66, 33)),
        "xml" | "xsl" => ("\u{e796}", Color::Rgb(228, 77, 38)),
        "csv" => ("\u{f1c3}", Color::Green),
        "sql" => ("\u{f1c0}", Color::Rgb(0, 116, 143)),
        "graphql" | "gql" => ("\u{e662}", Color::Rgb(229, 53, 171)),
        "proto" => ("\u{f481}", Color::Cyan),
        // Build & package
        "dockerfile" => ("\u{e7b0}", Color::Rgb(56, 134, 200)),
        "lock" => ("\u{f023}", Color::DarkGray),
        // Media
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" | "tiff"
            => ("\u{f1c5}", Color::Rgb(160, 100, 210)),
        "mp3" | "wav" | "flac" | "ogg" | "aac" | "m4a" | "wma"
            => ("\u{f1c7}", Color::Rgb(255, 87, 51)),
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v"
            => ("\u{f1c8}", Color::Rgb(253, 216, 53)),
        // Archives
        "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" | "zst" | "deb" | "rpm"
            => ("\u{f1c6}", Color::Rgb(175, 135, 0)),
        // Fonts
        "ttf" | "otf" | "woff" | "woff2" => ("\u{f031}", Color::White),
        // Misc
        "bin" | "exe" | "so" | "dylib" | "dll" => ("\u{f013}", Color::Green),
        _ if filename.starts_with('.') => ("\u{f013}", Color::DarkGray),
        _ => ("\u{f15b}", Color::DarkGray),
    }
}

fn fuzzy_match(query: &str, files: &[FileEntry], limit: usize) -> Vec<MatchEntry> {
    if limit == 0 {
        return Vec::new();
    }

    if query.is_empty() {
        return files
            .iter()
            .enumerate()
            .take(limit)
            .map(|(index, _)| MatchEntry {
                index,
                score: 100.0,
            })
            .collect();
    }

    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for (index, entry) in files.iter().enumerate() {
        // ratio returns 0.0-1.0, multiply by 100 for percentage
        let score = ratio(query_lower.chars(), entry.name_lower.chars()) * 100.0;
        if score > 35.0 {
            results.push(MatchEntry { index, score });
        }
    }

    if results.len() > limit {
        results.select_nth_unstable_by(limit, |a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
        });
        results.truncate(limit);
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
    });
    results
}

fn is_text_mime(mime_type: &str) -> bool {
    if mime_type.starts_with("text/") && mime_type != "text/html" {
        return true;
    }

    if let Some(subtype) = mime_type.strip_prefix("application/") {
        if subtype.ends_with("+json")
            || subtype.ends_with("+xml")
            || subtype.ends_with("+yaml")
            || subtype.ends_with("+toml")
        {
            return true;
        }
    }

    matches!(
        mime_type,
        "application/json"
            | "application/xml"
            | "application/x-yaml"
            | "application/yaml"
            | "application/toml"
            | "application/x-toml"
            | "application/javascript"
            | "application/x-shellscript"
            | "application/x-sh"
    )
}

fn is_text_extension(path: &str) -> bool {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if file_name.starts_with('.') && ext.is_empty() {
        return true;
    }

    matches!(
        ext.as_str(),
        "txt"
            | "md"
            | "rs"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "cxx"
            | "py"
            | "js"
            | "ts"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "go"
            | "java"
            | "sh"
            | "bash"
            | "zsh"
            | "css"
            | "xml"
            | "csv"
    )
}

fn normalize_socket(socket: String) -> String {
    if socket.contains(':') {
        socket
    } else {
        format!("unix:{}", socket)
    }
}

fn find_kitty_sockets() -> Vec<(SystemTime, String)> {
    let mut sockets = Vec::new();
    let entries = match fs::read_dir("/tmp") {
        Ok(entries) => entries,
        Err(_) => return sockets,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(name) => name,
            None => continue,
        };

        if !file_name.starts_with("kitty.sock-") {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        if !metadata.file_type().is_socket() {
            continue;
        }

        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        sockets.push((modified, path.to_string_lossy().to_string()));
    }

    sockets
}

fn kitty_socket_candidates() -> Vec<String> {
    let mut candidates = Vec::new();

    if let Some(socket) = env::var("FFP_KITTY_SOCKET")
        .ok()
        .or_else(|| env::var("KITTY_LISTEN_ON").ok())
    {
        candidates.push(normalize_socket(socket));
    }

    let mut sockets = find_kitty_sockets();
    sockets.sort_by_key(|(modified, _)| *modified);
    sockets.reverse();
    for (_, path) in sockets {
        let candidate = format!("unix:{}", path);
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    candidates
}

fn try_kitty_launch(socket: &str, path: &str) -> bool {
    Command::new("kitten")
        .args([
            "@",
            "--to",
            socket,
            "launch",
            "--type=os-window",
            "nvim",
            path,
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn open_text_file(path: &str) -> Result<(), String> {
    for socket in kitty_socket_candidates() {
        if try_kitty_launch(&socket, path) {
            return Ok(());
        }
    }

    Command::new("kitty")
        .args(["nvim", path])
        .spawn()
        .map_err(|e| format!("Failed to open in kitty: {}", e))?;

    Ok(())
}

fn run_profile() -> io::Result<()> {
    let files = Arc::new(Mutex::new(Vec::new()));
    let loading = Arc::new(Mutex::new(true));

    let start = Instant::now();
    load_files(Arc::clone(&files), Arc::clone(&loading));
    let load_duration = start.elapsed();

    let files = files.lock().unwrap();
    let file_count = files.len();
    eprintln!("profile: loaded {} files in {:?}", file_count, load_duration);

    let empty_start = Instant::now();
    let empty_matches = fuzzy_match("", &files, MATCH_LIMIT);
    eprintln!(
        "profile: query <empty> -> {} matches in {:?}",
        empty_matches.len(),
        empty_start.elapsed()
    );

    let queries = env::var("FFP_PROFILE_QUERIES").unwrap_or_else(|_| "rs,main,doc".to_string());
    for query in queries.split(',').map(str::trim).filter(|q| !q.is_empty()) {
        let start = Instant::now();
        let matches = fuzzy_match(query, &files, MATCH_LIMIT);
        eprintln!(
            "profile: query {:?} -> {} matches in {:?}",
            query,
            matches.len(),
            start.elapsed()
        );
    }

    // Profile image loading
    eprintln!("\n=== Image Loading Profile ===");
    let image_files: Vec<_> = files.iter()
        .filter(|f| is_image_file(&f.path))
        .take(5)
        .collect();

    if image_files.is_empty() {
        eprintln!("No image files found in recent files");
    } else {
        for file in &image_files {
            let start = Instant::now();
            let result = load_thumbnail(&file.path);
            let duration = start.elapsed();

            match result {
                Some(img) => {
                    eprintln!(
                        "profile: image {:?} -> {}x{} thumb, {} bytes in {:?}",
                        file.name,
                        img.width,
                        img.height,
                        img.data.len(),
                        duration
                    );
                }
                None => {
                    eprintln!("profile: image {:?} -> FAILED in {:?}", file.name, duration);
                }
            }
        }

        // Profile cached access
        eprintln!("\n=== Cached Image Access ===");
        let mut cache = ImageCache::new();

        // First load (cold)
        if let Some(first) = image_files.first() {
            let start = Instant::now();
            if let Some(img) = load_thumbnail(&first.path) {
                let load_time = start.elapsed();
                cache.insert(first.path.clone(), img);
                eprintln!("profile: cold load {:?} in {:?}", first.name, load_time);

                // Cached access (hot)
                let start = Instant::now();
                let _ = cache.get(&first.path);
                let cache_time = start.elapsed();
                eprintln!("profile: hot cache access {:?} in {:?}", first.name, cache_time);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_empty_query_returns_first_items() {
        let files = vec![
            FileEntry::new("first.txt".to_string()),
            FileEntry::new("second.txt".to_string()),
            FileEntry::new("third.txt".to_string()),
        ];

        let matches = fuzzy_match("", &files, 2);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].index, 0);
        assert_eq!(matches[1].index, 1);
    }

    #[test]
    fn fuzzy_match_handles_special_characters() {
        let files = vec![FileEntry::new("foo[bar].txt".to_string())];

        let matches = fuzzy_match("foo[bar].txt", &files, 10);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].index, 0);
    }

    #[test]
    fn fuzzy_match_handles_unicode() {
        let files = vec![FileEntry::new("Résumé.txt".to_string())];

        let matches = fuzzy_match("résumé", &files, 10);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].index, 0);
    }
}

fn open_file(path: &str) -> Result<(), String> {
    // Log to file for debugging
    use std::io::Write;
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/ffp.log")
        .ok();

    if let Some(ref mut f) = log {
        let _ = writeln!(f, "=== Opening: {} ===", path);
    }

    let mime_output = Command::new("xdg-mime")
        .args(["query", "filetype", path])
        .output();

    let mime_type = mime_output
        .as_ref()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    if let Some(ref mut f) = log {
        let _ = writeln!(f, "MIME: {}", mime_type);
    }

    let is_text = if mime_type.is_empty() {
        is_text_extension(path)
    } else {
        is_text_mime(&mime_type)
    };

    if let Some(ref mut f) = log {
        let _ = writeln!(f, "is_text: {}", is_text);
    }

    if is_text {
        if let Some(ref mut f) = log {
            let _ = writeln!(f, "Opening as text file");
        }
        open_text_file(path)
    } else {
        if let Some(ref mut f) = log {
            let _ = writeln!(f, "Opening with xdg-open via setsid");
        }
        // Use setsid to fully detach the process so it survives after ffp exits
        let result = Command::new("setsid")
            .args(["--fork", "xdg-open", path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match result {
            Ok(child) => {
                if let Some(ref mut f) = log {
                    let _ = writeln!(f, "setsid spawned, pid: {:?}", child.id());
                }
                Ok(())
            }
            Err(e) => {
                if let Some(ref mut f) = log {
                    let _ = writeln!(f, "setsid failed: {}", e);
                }
                Err(format!("Failed to xdg-open: {}", e))
            }
        }
    }
}

fn build_file_items<'a>(
    matches: &[MatchEntry],
    files: &[FileEntry],
) -> Vec<ListItem<'a>> {
    matches
        .iter()
        .filter_map(|matched| {
            let entry = files.get(matched.index)?;
            let fname = &entry.name;
            let dir = &entry.dir;
            let (icon, icon_color) = get_icon(fname);
            let time = time_ago(entry.mtime);

            // Split filename into stem and extension
            let p = Path::new(fname);
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(fname);
            let ext_part = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|e| format!(".{}", e))
                .unwrap_or_default();

            Some(ListItem::new(Line::from(vec![
                Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
                Span::styled(
                    stem.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(ext_part, Style::default().fg(icon_color)),
                Span::styled(
                    format!(" {}", time),
                    Style::default().fg(Color::Rgb(80, 80, 80)),
                ),
                Span::raw(" "),
                Span::styled(dir.to_string(), Style::default().fg(Color::DarkGray)),
            ])))
        })
        .collect()
}

fn ui(frame: &mut Frame, app: &App) -> Option<Rect> {
    let area = frame.area();
    let wide_mode = area.width >= 100;
    let dual_columns = area.width >= 140;

    // Build file list items (all matches, scrolling handled by ListState)
    let (score_items, time_items, file_count) = {
        let files = app.files.lock().unwrap();
        let count = files.len();
        let score_list = build_file_items(&app.matches, &files);
        let time_list = build_file_items(&app.matches_by_time, &files);
        (score_list, time_list, count)
    };

    // ListState for scrolling in the active column
    let highlight_style = Style::default()
        .bg(Color::Rgb(40, 44, 52))
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let mut score_state = ListState::default();
    let mut time_state = ListState::default();
    if app.active_column == 0 {
        score_state.select(Some(app.selected));
    } else {
        time_state.select(Some(app.selected));
    }

    // Build preview widget (for text content)
    let (preview_lines, needs_graphics): (Vec<Line>, bool) = match &app.preview_content {
        PreviewContent::Text(lines) => (lines.clone(), false),
        PreviewContent::Image(_) => (vec![], true),
        PreviewContent::Video(frames) => (
            vec![Line::from(Span::styled(
                format!(" {}/{}", app.video_frame + 1, frames.len()),
                Style::default().fg(Color::Green),
            ))],
            true,
        ),
        PreviewContent::VideoLoading => (
            vec![Line::from(Span::styled(
                "Loading video...",
                Style::default().fg(Color::Yellow),
            ))],
            false,
        ),
        PreviewContent::None => (
            vec![Line::from(Span::styled(
                "No file selected",
                Style::default().fg(Color::DarkGray),
            ))],
            false,
        ),
    };
    let preview_title = match &app.preview_content {
        PreviewContent::Video(_) | PreviewContent::VideoLoading => " Video ",
        PreviewContent::Image(_) => " Image ",
        _ => " Preview ",
    };
    let preview = Paragraph::new(preview_lines).block(
        Block::default()
            .borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Rgb(60, 60, 60)))
            .title(Span::styled(preview_title, Style::default().fg(Color::Yellow))),
    );

    // Status line
    let status = format!(
        "{} matches{} | {} files",
        app.matches.len(),
        if app.is_loading() { " (loading...)" } else { "" },
        file_count
    );
    let status_widget = Paragraph::new(status)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(ratatui::layout::Alignment::Center);

    // Help line
    let help = Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" open  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" cancel  "),
        Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::Cyan)),
        Span::raw(" nav  "),
        Span::styled("^U", Style::default().fg(Color::Cyan)),
        Span::raw(" clr"),
    ]);
    let help_widget = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(ratatui::layout::Alignment::Center);

    // Search input
    let input = Paragraph::new(app.query.as_str())
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Rgb(60, 60, 60)))
                .title(Span::styled(" \u{f002} ", Style::default().fg(Color::Cyan))),
        );

    let preview_area = if dual_columns {
        // Extra wide: dual file columns + preview
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(main_chunks[0]);

        // Split file area into two columns
        let file_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(left_chunks[1]);

        frame.render_widget(input, left_chunks[0]);
        frame.render_stateful_widget(
            List::new(score_items)
                .highlight_style(highlight_style)
                .highlight_symbol(" ")
                .block(
                    Block::default()
                        .borders(Borders::RIGHT).border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Rgb(60, 60, 60)))
                        .title(Span::styled(" Ranked ", Style::default().fg(Color::Cyan))),
                ),
            file_cols[0],
            &mut score_state,
        );
        frame.render_stateful_widget(
            List::new(time_items)
                .highlight_style(highlight_style)
                .highlight_symbol(" ")
                .block(
                    Block::default()
                        .title(Span::styled(" Recent ", Style::default().fg(Color::Cyan))),
                ),
            file_cols[1],
            &mut time_state,
        );
        frame.render_widget(status_widget, left_chunks[2]);
        frame.render_widget(help_widget, left_chunks[3]);
        frame.render_widget(preview, main_chunks[1]);

        frame.set_cursor_position((
            left_chunks[0].x + app.query.len() as u16 + 1,
            left_chunks[0].y + 1,
        ));

        main_chunks[1]
    } else if wide_mode {
        // Wide: side-by-side layout
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(main_chunks[0]);

        frame.render_widget(input, left_chunks[0]);
        frame.render_stateful_widget(
            List::new(score_items)
                .highlight_style(highlight_style)
                .highlight_symbol(" ")
                .block(Block::default()),
            left_chunks[1],
            &mut score_state,
        );
        frame.render_widget(status_widget, left_chunks[2]);
        frame.render_widget(help_widget, left_chunks[3]);
        frame.render_widget(preview, main_chunks[1]);

        frame.set_cursor_position((
            left_chunks[0].x + app.query.len() as u16 + 1,
            left_chunks[0].y + 1,
        ));

        main_chunks[1]
    } else {
        // Narrow: list and preview split vertically
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Percentage(50),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        frame.render_widget(input, chunks[0]);
        frame.render_stateful_widget(
            List::new(score_items)
                .highlight_style(highlight_style)
                .highlight_symbol(" ")
                .block(Block::default()),
            chunks[1],
            &mut score_state,
        );
        frame.render_widget(preview, chunks[2]);
        frame.render_widget(status_widget, chunks[3]);
        frame.render_widget(help_widget, chunks[4]);

        frame.set_cursor_position((
            chunks[0].x + app.query.len() as u16 + 1,
            chunks[0].y + 1,
        ));

        chunks[2]
    };

    if needs_graphics {
        Some(preview_area)
    } else {
        None
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|arg| arg == "--profile") {
        return run_profile();
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let mut chosen: Option<String> = None;

    let mut last_image_area: Option<Rect> = None;
    let mut last_rendered_path = String::new();
    let mut last_video_frame: usize = usize::MAX;

    loop {
        app.update_matches();
        app.update_preview();

        let mut image_area: Option<Rect> = None;
        terminal.draw(|f| {
            image_area = ui(f, &app);
        })?;

        // Render kitty graphics for image/video preview
        if let Some(area) = image_area {
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
            match &app.preview_content {
                PreviewContent::Image(ref img) => {
                    // Only re-render if the image changed
                    if app.preview_path != last_rendered_path {
                        let _ = render_kitty_image(img, inner);
                        last_rendered_path = app.preview_path.clone();
                    }
                    last_image_area = Some(area);
                }
                PreviewContent::Video(ref frames) => {
                    // Advance frame on timer
                    if app.video_frame_time.elapsed()
                        >= Duration::from_millis(VIDEO_FRAME_INTERVAL_MS)
                    {
                        app.video_frame = (app.video_frame + 1) % frames.len();
                        app.video_frame_time = Instant::now();
                    }
                    // Only re-render if frame changed
                    if app.video_frame != last_video_frame
                        || app.preview_path != last_rendered_path
                    {
                        if let Some(frame) = frames.get(app.video_frame) {
                            let _ = render_kitty_image(frame, inner);
                        }
                        last_video_frame = app.video_frame;
                        last_rendered_path = app.preview_path.clone();
                    }
                    last_image_area = Some(area);
                }
                _ => {}
            }
        } else if last_image_area.is_some() {
            let _ = clear_kitty_image();
            last_image_area = None;
            last_rendered_path.clear();
            last_video_frame = usize::MAX;
        }

        // Poll with shorter timeout for smooth video, longer for static content
        let poll_ms = match &app.preview_content {
            PreviewContent::Video(_) => 16, // ~60fps
            PreviewContent::VideoLoading => 50, // check for loaded frames frequently
            _ => 100,
        };
        if event::poll(Duration::from_millis(poll_ms))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => break,
                    (KeyCode::Enter, _) => {
                        if let Some(file) = app.selected_file() {
                            chosen = Some(file);
                        }
                        break;
                    }
                    (KeyCode::Down, _) | (KeyCode::Tab, KeyModifiers::NONE) => {
                        if app.selected < app.active_matches().len().saturating_sub(1) {
                            app.selected += 1;
                        }
                    }
                    (KeyCode::Up, _) | (KeyCode::BackTab, _) => {
                        if app.selected > 0 {
                            app.selected -= 1;
                        }
                    }
                    (KeyCode::Char('n'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                        if app.selected < app.active_matches().len().saturating_sub(1) {
                            app.selected += 1;
                        }
                    }
                    (KeyCode::Char('p'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                        if app.selected > 0 {
                            app.selected -= 1;
                        }
                    }
                    (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                        if app.active_column != 0 {
                            app.active_column = 0;
                            app.selected = 0;
                        }
                    }
                    (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                        if app.active_column != 1 {
                            app.active_column = 1;
                            app.selected = 0;
                        }
                    }
                    (KeyCode::Char('y'), KeyModifiers::CONTROL) => {
                        if let Some(file) = app.selected_file() {
                            let _ = Command::new("wl-copy")
                                .arg(&file)
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn();
                        }
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        app.query.clear();
                        app.selected = 0;
                    }
                    (KeyCode::Char('w'), KeyModifiers::CONTROL)
                    | (KeyCode::Backspace, KeyModifiers::ALT) => {
                        // Delete word
                        while app.query.ends_with(' ') {
                            app.query.pop();
                        }
                        while !app.query.is_empty() && !app.query.ends_with(' ') {
                            app.query.pop();
                        }
                        app.selected = 0;
                    }
                    (KeyCode::Backspace, KeyModifiers::NONE) => {
                        app.query.pop();
                        app.selected = 0;
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        app.query.push(c);
                        app.selected = 0;
                    }
                    _ => {}
                }
            }
        }
    }

    // Clear any kitty graphics before exiting
    let _ = clear_kitty_image();

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    // Open the file after terminal is restored
    if let Some(path) = chosen {
        if let Err(e) = open_file(&path) {
            let _ = notify_rust::Notification::new()
                .summary("File Picker")
                .body(&e)
                .show();
        }
    }

    Ok(())
}
