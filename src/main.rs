use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
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
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
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
const DISPLAY_LIMIT: usize = 30;
const FILE_BATCH_SIZE: usize = 256;
const IMAGE_CACHE_SIZE: usize = 10;
const THUMBNAIL_MAX_WIDTH: u32 = 400;
const THUMBNAIL_MAX_HEIGHT: u32 = 300;

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
}

struct MatchEntry {
    index: usize,
    score: f64,
}

enum PreviewContent {
    Text(Vec<String>),
    Image(CachedImage),
    None,
}

struct App {
    query: String,
    files: Arc<Mutex<Vec<FileEntry>>>,
    matches: Vec<MatchEntry>,
    selected: usize,
    loading: Arc<Mutex<bool>>,
    last_query: String,
    last_file_count: usize,
    preview_path: String,
    preview_content: PreviewContent,
    image_cache: ImageCache,
    preview_area: Option<Rect>,  // For kitty graphics positioning
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

        Self {
            query: String::new(),
            files,
            matches: Vec::new(),
            selected: 0,
            loading,
            last_query: String::new(),
            last_file_count: 0,
            preview_path: String::new(),
            preview_content: PreviewContent::None,
            image_cache: ImageCache::new(),
            preview_area: None,
        }
    }

    fn update_preview(&mut self) {
        let current_path = self.selected_file().unwrap_or_default();
        if current_path == self.preview_path {
            return; // Already cached
        }
        self.preview_path = current_path.clone();

        if current_path.is_empty() {
            self.preview_content = PreviewContent::None;
            return;
        }

        if is_image_file(&current_path) {
            // Check cache first
            if let Some(cached) = self.image_cache.get(&current_path) {
                self.preview_content = PreviewContent::Image(cached.clone());
                return;
            }
            // Load and cache image
            if let Some(img) = load_thumbnail(&current_path) {
                self.image_cache.insert(current_path.clone(), img.clone());
                self.preview_content = PreviewContent::Image(img);
            } else {
                self.preview_content = PreviewContent::Text(vec!["[Cannot load image]".to_string()]);
            }
        } else {
            self.preview_content = PreviewContent::Text(read_preview(&current_path, 40));
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
        self.last_query = self.query.clone();
        self.last_file_count = files_len;
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn selected_file(&self) -> Option<String> {
        let files = self.files.lock().unwrap();
        self.matches
            .get(self.selected)
            .and_then(|m| files.get(m.index))
            .map(|entry| entry.path.clone())
    }

    fn is_loading(&self) -> bool {
        *self.loading.lock().unwrap()
    }

    fn file_count(&self) -> usize {
        self.files.lock().unwrap().len()
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

        Self {
            path,
            name,
            name_lower,
            dir,
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
                // Truncate long lines
                if line.len() > 60 {
                    line = format!("{}...", &line[..57]);
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

    // Clear previous image in this area
    // Using kitty's delete command for images at specific position
    write!(stdout, "\x1b_Ga=d,d=a\x1b\\")?;

    // Encode image data as base64
    let b64_data = BASE64.encode(&img.data);

    // Calculate cell dimensions (assuming ~2:1 aspect ratio for terminal cells)
    let cols = area.width as u32;
    let rows = area.height as u32;

    // Send image in chunks (kitty protocol limit is 4096 bytes per chunk)
    let chunk_size = 4096;
    let chunks: Vec<&str> = b64_data.as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;
        if i == 0 {
            // First chunk: include image metadata
            write!(
                stdout,
                "\x1b_Ga=T,f=100,t=d,c={},r={},m={};{}\x1b\\",
                cols,
                rows,
                if is_last { 0 } else { 1 },
                chunk
            )?;
        } else {
            // Continuation chunks
            write!(
                stdout,
                "\x1b_Gm={};{}\x1b\\",
                if is_last { 0 } else { 1 },
                chunk
            )?;
        }
    }

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

fn get_icon(filename: &str) -> &'static str {
    let ext = Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Nerd Font icons using Unicode codepoints
    match ext.as_str() {
        "pdf" => "\u{f1c1}",           // nf-fa-file_pdf_o
        "doc" | "docx" => "\u{f1c2}",  // nf-fa-file_word_o
        "md" => "\u{e73e}",            // nf-dev-markdown
        "txt" => "\u{f15c}",           // nf-fa-file_text_o
        "cpp" | "cc" | "cxx" => "\u{e61d}", // nf-custom-cpp
        "c" | "h" => "\u{e61e}",       // nf-custom-c
        "py" => "\u{e73c}",            // nf-dev-python
        "js" => "\u{e74e}",            // nf-dev-javascript
        "ts" => "\u{e628}",            // nf-seti-typescript
        "rs" => "\u{e7a8}",            // nf-dev-rust
        "go" => "\u{e626}",            // nf-dev-go
        "java" => "\u{e738}",          // nf-dev-java
        "sh" | "bash" | "zsh" => "\u{e795}", // nf-dev-terminal
        "html" => "\u{e736}",          // nf-dev-html5
        "css" => "\u{e749}",           // nf-dev-css3
        "json" => "\u{e60b}",          // nf-seti-json
        "yaml" | "yml" => "\u{f481}",  // nf-oct-file_code
        "toml" => "\u{e6b2}",          // nf-seti-config
        "png" | "jpg" | "jpeg" | "gif" | "svg" => "\u{f1c5}", // nf-fa-file_image_o
        "mp3" | "wav" | "flac" => "\u{f1c7}", // nf-fa-file_audio_o
        "mp4" | "mkv" | "avi" => "\u{f1c8}", // nf-fa-file_video_o
        "zip" | "tar" | "gz" | "xz" => "\u{f1c6}", // nf-fa-file_archive_o
        _ if filename.starts_with('.') => "\u{f013}", // nf-fa-cog
        _ => "\u{f15b}",               // nf-fa-file_o
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
    if mime_type.starts_with("text/") {
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
            | "html"
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

fn ui(frame: &mut Frame, app: &App) -> Option<Rect> {
    let area = frame.area();
    let wide_mode = area.width >= 100;

    // Build file list items
    let (items, file_count): (Vec<ListItem>, usize) = {
        let files = app.files.lock().unwrap();
        let count = files.len();
        let list: Vec<ListItem> = app
            .matches
            .iter()
            .enumerate()
            .take(DISPLAY_LIMIT)
            .filter_map(|(i, matched)| {
                let entry = files.get(matched.index)?;
                let fname = entry.name.as_str();
                let dir = entry.dir.as_str();
                let icon = get_icon(fname);

                let style = if i == app.selected {
                    Style::default()
                        .bg(Color::Rgb(40, 44, 52))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let prefix = if i == app.selected { " " } else { "  " };

                Some(ListItem::new(Line::from(vec![
                    Span::styled(format!("{}{} ", prefix, icon), Style::default().fg(Color::Cyan)),
                    Span::styled(fname.to_string(), style.add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(dir.to_string(), Style::default().fg(Color::DarkGray)),
                ]))
                .style(style))
            })
            .collect();
        (list, count)
    };

    // Build preview widget (for text content)
    let (preview_lines, is_image): (Vec<Line>, bool) = match &app.preview_content {
        PreviewContent::Text(lines) => (
            lines.iter()
                .map(|s| Line::from(Span::styled(s.as_str(), Style::default().fg(Color::Gray))))
                .collect(),
            false,
        ),
        PreviewContent::Image(_) => (
            vec![Line::from(Span::styled("[Image preview below]", Style::default().fg(Color::Green)))],
            true,
        ),
        PreviewContent::None => (
            vec![Line::from(Span::styled("No file selected", Style::default().fg(Color::DarkGray)))],
            false,
        ),
    };
    let preview = Paragraph::new(preview_lines)
        .block(Block::default().borders(Borders::ALL).title(Span::styled(
            if is_image { "  Image " } else { " Preview " },
            Style::default().fg(Color::Yellow),
        )));

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
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
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
                .borders(Borders::ALL)
                .title(Span::styled(" ", Style::default().fg(Color::Cyan))),
        );

    let preview_area = if wide_mode {
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
        frame.render_widget(List::new(items).block(Block::default()), left_chunks[1]);
        frame.render_widget(status_widget, left_chunks[2]);
        frame.render_widget(help_widget, left_chunks[3]);
        frame.render_widget(preview, main_chunks[1]);

        frame.set_cursor_position((
            left_chunks[0].x + app.query.len() as u16 + 1,
            left_chunks[0].y + 1,
        ));

        main_chunks[1]
    } else {
        // Narrow: preview at bottom
        // List height matches actual items, preview gets remaining space
        let list_height = (items.len() as u16).max(1);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),           // input
                Constraint::Length(list_height), // file list (sized to content)
                Constraint::Min(3),              // preview (gets remaining space)
                Constraint::Length(1),           // status
                Constraint::Length(1),           // help
            ])
            .split(area);

        frame.render_widget(input, chunks[0]);
        frame.render_widget(List::new(items).block(Block::default()), chunks[1]);
        frame.render_widget(preview, chunks[2]);
        frame.render_widget(status_widget, chunks[3]);
        frame.render_widget(help_widget, chunks[4]);

        frame.set_cursor_position((
            chunks[0].x + app.query.len() as u16 + 1,
            chunks[0].y + 1,
        ));

        chunks[2]
    };

    // Return preview area if we have an image to render
    if is_image {
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

    loop {
        app.update_matches();
        app.update_preview();

        let mut image_area: Option<Rect> = None;
        terminal.draw(|f| {
            image_area = ui(f, &app);
        })?;

        // Render kitty graphics for image preview
        if let Some(area) = image_area {
            if let PreviewContent::Image(ref img) = app.preview_content {
                // Adjust area for border (1 cell padding)
                let inner = Rect {
                    x: area.x + 1,
                    y: area.y + 1,
                    width: area.width.saturating_sub(2),
                    height: area.height.saturating_sub(2),
                };
                let _ = render_kitty_image(img, inner);
                last_image_area = Some(area);
            }
        } else if last_image_area.is_some() {
            // Clear previous image when switching to non-image
            let _ = clear_kitty_image();
            last_image_area = None;
        }

        // Poll for events with timeout (for loading updates)
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => break,
                    (KeyCode::Enter, _) => {
                        if let Some(file) = app.selected_file() {
                            chosen = Some(file);
                        }
                        break;
                    }
                    (KeyCode::Down, _) | (KeyCode::Tab, KeyModifiers::NONE) => {
                        if app.selected < app.matches.len().saturating_sub(1) {
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
                        if app.selected < app.matches.len().saturating_sub(1) {
                            app.selected += 1;
                        }
                    }
                    (KeyCode::Char('p'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                        if app.selected > 0 {
                            app.selected -= 1;
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
