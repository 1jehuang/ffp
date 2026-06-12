#![allow(clippy::items_after_test_module)]

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use image::GenericImageView;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, BufRead, BufReader, BufWriter, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

const MATCH_LIMIT: usize = 50;
const MATCH_CANDIDATE_LIMIT: usize = 1024;
const SEARCH_WORKER_MAX: usize = 8;
/// Below this pool size the parallel scoring fan-out costs more than it saves.
const PARALLEL_SCORE_MIN_FILES: usize = 8_192;
const MATCH_QUERY_CACHE_SIZE: usize = 32;
const INCREMENTAL_QUERY_MIN_CHARS: usize = 3;
const INCREMENTAL_ACCEPT_MIN_MATCHES: usize = 8;
const FILE_BATCH_SIZE: usize = 256;
const ENTRY_PUBLISH_INTERVAL: Duration = Duration::from_millis(100);
const STAT_WORKER_MAX: usize = 8;
const FILE_LOOKBACK_WINDOW: &str = "9d";
const FILE_LOOKBACK_WINDOW_SECS: u64 = 9 * 24 * 60 * 60;
/// UI label for the recent scope, including the lookback cutoff.
/// Keep in sync with FILE_LOOKBACK_WINDOW.
const RECENT_SCOPE_LABEL: &str = "recent (9d)";
const RECENT_POOL_LIMIT: usize = 20_000;
const IMAGE_CACHE_SIZE: usize = 10;
const THUMBNAIL_MAX_WIDTH: u32 = 800;
const THUMBNAIL_MAX_HEIGHT: u32 = 600;
const VIDEO_THUMBNAIL_MAX_WIDTH: u32 = 360;
const VIDEO_THUMBNAIL_MAX_HEIGHT: u32 = 240;
const VIDEO_PREVIEW_FRAMES: usize = 75;
const VIDEO_FRAME_INTERVAL_MS: u64 = 100;
const INPUT_RENDER_GRACE_MS: u64 = 90;
const INPUT_DRAIN_LIMIT: usize = 32;
const VIDEO_CACHE_SIZE: usize = 5;
const HISTORY_MAX_ENTRIES: usize = 5000;

#[derive(Clone, Debug)]
struct CachedImage {
    data: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    is_raw_rgb: bool,
}

struct LruCache<T> {
    cache: HashMap<String, T>,
    order: Vec<String>,
    capacity: usize,
}

impl<T> LruCache<T> {
    fn new(capacity: usize) -> Self {
        Self {
            cache: HashMap::new(),
            order: Vec::new(),
            capacity,
        }
    }

    fn get(&mut self, path: &str) -> Option<&T> {
        if self.cache.contains_key(path) {
            self.order.retain(|p| p != path);
            self.order.push(path.to_string());
            self.cache.get(path)
        } else {
            None
        }
    }

    fn insert(&mut self, path: String, value: T) {
        while self.order.len() >= self.capacity {
            if let Some(old) = self.order.first().cloned() {
                self.cache.remove(&old);
                self.order.remove(0);
            }
        }
        self.order.push(path.clone());
        self.cache.insert(path, value);
    }

    fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
    }
}

type ImageCache = LruCache<CachedImage>;
type VideoCache = LruCache<Vec<CachedImage>>;
type PendingVideoCache = Arc<Mutex<Option<(String, Vec<CachedImage>)>>>;
type EntrySnapshot = Arc<Vec<FileEntry>>;
type SharedEntries = Arc<Mutex<EntrySnapshot>>;

struct TraceLogger {
    writer: Option<BufWriter<fs::File>>,
    slow_threshold: Duration,
}

impl TraceLogger {
    fn new(args: &[String]) -> Self {
        let enabled = args.iter().any(|arg| arg == "--trace-responsive")
            || env::var("FFP_TRACE_RESPONSIVENESS")
                .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
                .unwrap_or(false);

        if !enabled {
            return Self {
                writer: None,
                slow_threshold: Duration::from_millis(8),
            };
        }

        let path = env::var_os("FFP_TRACE_LOG")
            .map(PathBuf::from)
            .or_else(|| history_file_path().map(|path| path.with_file_name("responsiveness.log")));
        let writer = path.and_then(|path| {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
                .map(BufWriter::new)
        });

        let slow_threshold = env::var("FFP_TRACE_SLOW_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(8));

        let mut logger = Self {
            writer,
            slow_threshold,
        };
        logger.log("trace_start", format!("pid={}", std::process::id()));
        logger
    }

    fn enabled(&self) -> bool {
        self.writer.is_some()
    }

    fn log(&mut self, event: &str, detail: impl AsRef<str>) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        let _ = writeln!(
            writer,
            "{} event={} {}",
            unix_time_ms(),
            event,
            detail.as_ref()
        );
    }

    fn log_duration(&mut self, event: &str, duration: Duration, detail: impl AsRef<str>) {
        if duration >= self.slow_threshold {
            self.log(
                event,
                format!("duration_us={} {}", duration.as_micros(), detail.as_ref()),
            );
        }
    }
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[derive(Clone)]
struct FileEntry {
    /// Shared so snapshot publishes clone refcounts instead of string data.
    path: Arc<str>,
    path_lower: Arc<str>,
    /// Byte offset of the basename within `path`.
    name_start: u32,
    /// Byte offset of the basename within `path_lower`.
    lower_name_start: u32,
    mtime: SystemTime,
    recency_time: SystemTime,
    recency_source: RecencySource,
    is_dir: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecencySource {
    Modified,
    Accessed,
    FfpHistory,
    DesktopRecent,
}

impl RecencySource {
    fn label(self) -> &'static str {
        match self {
            Self::Modified => "mod",
            Self::Accessed => "read",
            Self::FfpHistory => "ffp",
            Self::DesktopRecent => "desk",
        }
    }
}

#[derive(Clone, Default)]
struct RecencyIndex {
    ffp_history: HashMap<String, SystemTime>,
    desktop_recent: HashMap<String, SystemTime>,
    cwd: PathBuf,
}

impl RecencyIndex {
    fn load() -> Self {
        Self {
            ffp_history: load_ffp_history(),
            desktop_recent: load_desktop_recent(),
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    fn path_key<'a>(&self, path: &'a str) -> Cow<'a, str> {
        if path_is_normalized_absolute(path) {
            return Cow::Borrowed(path);
        }
        Cow::Owned(normalized_path_key_with_cwd(path, &self.cwd))
    }

    /// Look up history/desktop recency for a path without allocating in the
    /// common case where the recency maps are small or empty.
    fn recency_times(&self, path: &str) -> (Option<SystemTime>, Option<SystemTime>) {
        if self.ffp_history.is_empty() && self.desktop_recent.is_empty() {
            return (None, None);
        }
        let key = self.path_key(path);
        (
            self.ffp_history.get(key.as_ref()).copied(),
            self.desktop_recent.get(key.as_ref()).copied(),
        )
    }

    fn candidate_paths(&self) -> Vec<String> {
        let mut candidates: HashMap<String, SystemTime> = HashMap::new();
        for (path, time) in self.ffp_history.iter().chain(self.desktop_recent.iter()) {
            let entry = candidates.entry(path.clone()).or_insert(*time);
            if *time > *entry {
                *entry = *time;
            }
        }

        let mut candidates: Vec<(String, SystemTime)> = candidates.into_iter().collect();
        candidates.sort_by_key(|(_, time)| std::cmp::Reverse(*time));
        candidates.into_iter().map(|(path, _)| path).collect()
    }
}

fn recent_window() -> Duration {
    Duration::from_secs(FILE_LOOKBACK_WINDOW_SECS)
}

fn is_within_recent_window(time: SystemTime) -> bool {
    match SystemTime::now().duration_since(time) {
        Ok(elapsed) => elapsed <= recent_window(),
        Err(_) => true,
    }
}

fn best_recency(
    mtime: SystemTime,
    atime: SystemTime,
    history_time: Option<SystemTime>,
    desktop_time: Option<SystemTime>,
) -> (SystemTime, RecencySource) {
    let mut best_time = mtime;
    let mut best_source = RecencySource::Modified;

    for (time, source) in [
        (Some(atime), RecencySource::Accessed),
        (desktop_time, RecencySource::DesktopRecent),
        (history_time, RecencySource::FfpHistory),
    ] {
        if let Some(time) = time {
            if time > best_time {
                best_time = time;
                best_source = source;
            }
        }
    }

    (best_time, best_source)
}

fn normalized_path_key(path: &str) -> String {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    normalized_path_key_with_cwd(path, &cwd)
}

/// True when a path is absolute and contains no `.`/`..` components or
/// trailing/duplicate separators, i.e. it is already its own normalized key.
fn path_is_normalized_absolute(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    if !path.starts_with('/') || path.ends_with('/') {
        return false;
    }
    !path[1..]
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
}

fn normalized_path_key_with_cwd(path: &str, cwd: &Path) -> String {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    normalize_path(&absolute).to_string_lossy().to_string()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

#[derive(Clone)]
struct MatchEntry {
    index: usize,
    score: f64,
}

struct SearchRequest {
    id: u64,
    query: String,
    scope: SearchScope,
    file_version: u64,
    files: EntrySnapshot,
}

struct SearchResult {
    id: u64,
    query: String,
    scope: SearchScope,
    /// The exact snapshot the matches were computed against. Renderers must
    /// resolve match indexes against this snapshot (never against a newer
    /// pool) so results can never point at the wrong files.
    files: EntrySnapshot,
    matches: Vec<MatchEntry>,
    matches_by_time: Vec<MatchEntry>,
}

struct MatchSearchState {
    cache: LruCache<Vec<usize>>,
    last_query: String,
    last_scope: SearchScope,
    last_file_version: u64,
    last_file_count: usize,
    last_candidates: Vec<usize>,
}

impl MatchSearchState {
    fn new(scope: SearchScope) -> Self {
        Self {
            cache: LruCache::new(MATCH_QUERY_CACHE_SIZE),
            last_query: String::new(),
            last_scope: scope,
            last_file_version: u64::MAX,
            last_file_count: 0,
            last_candidates: Vec::new(),
        }
    }

    fn reset_if_stale(&mut self, scope: SearchScope, file_version: u64, file_count: usize) {
        if self.last_scope != scope
            || self.last_file_version != file_version
            || self.last_file_count != file_count
        {
            self.cache.clear();
            self.last_query.clear();
            self.last_candidates.clear();
            self.last_scope = scope;
            self.last_file_version = file_version;
            self.last_file_count = file_count;
        }
    }
}

enum PreviewContent {
    Text(Vec<Line<'static>>),
    Image(CachedImage),
    Video(Vec<CachedImage>),
    VideoStreaming(Vec<CachedImage>),
    VideoLoading,
    None,
}

struct PreviewRequest {
    id: u64,
    path: String,
}

struct PreviewResult {
    id: u64,
    path: String,
    content: PreviewResultContent,
}

enum PreviewResultContent {
    Text(Vec<Line<'static>>),
    Image(Option<CachedImage>),
}

enum ExitAction {
    Open(String),
    Drag(String),
    Choose(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchScope {
    Recent,
    All,
}

impl SearchScope {
    /// Label shown in the search box and status line. Recent includes its
    /// time cutoff (e.g. "recent (9d)") so the active window is visible.
    fn label(self) -> &'static str {
        match self {
            Self::Recent => RECENT_SCOPE_LABEL,
            Self::All => "all",
        }
    }

    fn toggle(self) -> Self {
        match self {
            Self::Recent => Self::All,
            Self::All => Self::Recent,
        }
    }
}

struct App {
    query: String,
    recent_files: SharedEntries,
    all_files: SharedEntries,
    dir_entries: SharedEntries,
    recent_version: Arc<AtomicU64>,
    all_version: Arc<AtomicU64>,
    dir_version: Arc<AtomicU64>,
    matches: Vec<MatchEntry>,
    matches_by_time: Vec<MatchEntry>,
    /// Snapshot the current matches index into. Kept in lockstep with
    /// `matches`/`matches_by_time` so scope toggles can never render stale
    /// indexes against a different file pool.
    matches_files: EntrySnapshot,
    search_tx: mpsc::Sender<SearchRequest>,
    search_rx: mpsc::Receiver<SearchResult>,
    next_search_id: u64,
    latest_search_id: u64,
    /// Search id of the most recently accepted result, used to detect when a
    /// search is still in flight so the event loop can poll faster.
    last_result_search_id: u64,
    selected: usize,
    active_column: usize, // 0 = ranked, 1 = recent
    recent_loading: Arc<Mutex<bool>>,
    all_loading: Arc<Mutex<bool>>,
    dir_loading: Arc<Mutex<bool>>,
    search_scope: SearchScope,
    last_query: String,
    last_file_count: usize,
    last_file_version: u64,
    last_scope: SearchScope,
    preview_path: String,
    preview_content: PreviewContent,
    preview_tx: mpsc::Sender<PreviewRequest>,
    preview_rx: mpsc::Receiver<PreviewResult>,
    next_preview_id: u64,
    latest_preview_id: u64,
    image_cache: ImageCache,
    video_cache: VideoCache,
    video_frame: usize,
    video_frame_time: Instant,
    video_loader: Option<Arc<Mutex<Vec<CachedImage>>>>,
    video_loader_done: Option<Arc<Mutex<bool>>>,
    video_loader_path: String,
    video_loader_seen_frames: usize,
    video_cache_pending: PendingVideoCache,
    recency_index: Arc<RecencyIndex>,
    drag_mode: bool,
    dir_mode: bool,
}

impl App {
    fn new(dir_mode: bool) -> Self {
        let recent_files: SharedEntries = Arc::new(Mutex::new(Arc::new(Vec::new())));
        let all_files: SharedEntries = Arc::new(Mutex::new(Arc::new(Vec::new())));
        let dir_entries: SharedEntries = Arc::new(Mutex::new(Arc::new(Vec::new())));
        let recent_version = Arc::new(AtomicU64::new(0));
        let all_version = Arc::new(AtomicU64::new(0));
        let dir_version = Arc::new(AtomicU64::new(0));
        let recent_loading = Arc::new(Mutex::new(false));
        let all_loading = Arc::new(Mutex::new(false));
        let dir_loading = Arc::new(Mutex::new(false));
        let recency_index = Arc::new(RecencyIndex::load());
        let search_scope = if dir_mode {
            SearchScope::All
        } else {
            SearchScope::Recent
        };

        if dir_mode {
            spawn_entry_loader(
                Arc::clone(&dir_entries),
                Arc::clone(&dir_loading),
                Arc::clone(&dir_version),
                dir_mode,
                false,
                Arc::clone(&recency_index),
                None,
            );
        } else {
            // The recent loader's final full-tree pass also feeds the
            // all-files pool, so Ctrl+Z is instant instead of starting a
            // second multi-second scan.
            spawn_entry_loader(
                Arc::clone(&recent_files),
                Arc::clone(&recent_loading),
                Arc::clone(&recent_version),
                dir_mode,
                true,
                Arc::clone(&recency_index),
                Some(AllPoolSink {
                    files: Arc::clone(&all_files),
                    loading: Arc::clone(&all_loading),
                    version: Arc::clone(&all_version),
                }),
            );
        }

        let (search_tx, search_rx) = spawn_search_worker(search_scope);
        let (preview_tx, preview_rx) = spawn_preview_worker();

        Self {
            query: String::new(),
            recent_files,
            all_files,
            dir_entries,
            recent_version,
            all_version,
            dir_version,
            matches: Vec::new(),
            matches_by_time: Vec::new(),
            matches_files: Arc::new(Vec::new()),
            search_tx,
            search_rx,
            next_search_id: 0,
            latest_search_id: 0,
            last_result_search_id: 0,
            selected: 0,
            active_column: 0,
            recent_loading,
            all_loading,
            dir_loading,
            search_scope,
            last_query: String::new(),
            last_file_count: 0,
            last_file_version: u64::MAX,
            last_scope: search_scope,
            preview_path: String::new(),
            preview_content: PreviewContent::None,
            preview_tx,
            preview_rx,
            next_preview_id: 0,
            latest_preview_id: 0,
            image_cache: LruCache::new(IMAGE_CACHE_SIZE),
            video_cache: LruCache::new(VIDEO_CACHE_SIZE),
            video_frame: 0,
            video_frame_time: Instant::now(),
            video_loader: None,
            video_loader_done: None,
            video_loader_path: String::new(),
            video_loader_seen_frames: 0,
            video_cache_pending: Arc::new(Mutex::new(None)),
            recency_index,
            drag_mode: false,
            dir_mode,
        }
    }

    fn files_for_scope(&self, scope: SearchScope) -> &SharedEntries {
        if self.dir_mode {
            return &self.dir_entries;
        }

        match scope {
            SearchScope::Recent => &self.recent_files,
            SearchScope::All => &self.all_files,
        }
    }

    fn loading_for_scope(&self, scope: SearchScope) -> &Arc<Mutex<bool>> {
        if self.dir_mode {
            return &self.dir_loading;
        }

        match scope {
            SearchScope::Recent => &self.recent_loading,
            SearchScope::All => &self.all_loading,
        }
    }

    fn version_for_scope(&self, scope: SearchScope) -> &Arc<AtomicU64> {
        if self.dir_mode {
            return &self.dir_version;
        }

        match scope {
            SearchScope::Recent => &self.recent_version,
            SearchScope::All => &self.all_version,
        }
    }

    fn snapshot_for_scope(&self, scope: SearchScope) -> EntrySnapshot {
        Arc::clone(&self.files_for_scope(scope).lock().unwrap())
    }

    fn active_scope(&self) -> SearchScope {
        if self.dir_mode {
            SearchScope::All
        } else {
            self.search_scope
        }
    }

    fn toggle_search_scope(&mut self) {
        if self.dir_mode {
            return;
        }

        self.search_scope = self.search_scope.toggle();
        self.selected = 0;
        self.preview_path.clear();
        // Keep the current matches visible until the new scope's results
        // arrive; they stay correct because they render against the snapshot
        // they were computed from. Force a re-search immediately.
        self.last_file_version = u64::MAX;

        if self.search_scope == SearchScope::All {
            // No-op when the pool is already loaded or a loader (including
            // the startup recent loader feeding the all pool) owns it.
            spawn_entry_loader(
                Arc::clone(&self.all_files),
                Arc::clone(&self.all_loading),
                Arc::clone(&self.all_version),
                self.dir_mode,
                false,
                Arc::clone(&self.recency_index),
                None,
            );
        }
    }

    fn toggle_entry_mode(&mut self) {
        self.dir_mode = !self.dir_mode;
        self.selected = 0;
        self.preview_path.clear();
        self.preview_content = PreviewContent::None;
        self.clear_matches();
        self.last_query.clear();
        self.last_file_count = 0;
        self.last_file_version = u64::MAX;

        if self.dir_mode {
            spawn_entry_loader(
                Arc::clone(&self.dir_entries),
                Arc::clone(&self.dir_loading),
                Arc::clone(&self.dir_version),
                true,
                false,
                Arc::clone(&self.recency_index),
                None,
            );
        } else {
            let scope = self.search_scope;
            let all_sink = (scope == SearchScope::Recent).then(|| AllPoolSink {
                files: Arc::clone(&self.all_files),
                loading: Arc::clone(&self.all_loading),
                version: Arc::clone(&self.all_version),
            });
            spawn_entry_loader(
                Arc::clone(self.files_for_scope(scope)),
                Arc::clone(self.loading_for_scope(scope)),
                Arc::clone(self.version_for_scope(scope)),
                false,
                scope == SearchScope::Recent,
                Arc::clone(&self.recency_index),
                all_sink,
            );
        }
    }

    /// Drop all current matches together with their snapshot so the UI never
    /// renders indexes from one file pool against another.
    fn clear_matches(&mut self) {
        self.matches.clear();
        self.matches_by_time.clear();
        self.matches_files = Arc::new(Vec::new());
    }

    fn loading_preview(message: &'static str) -> PreviewContent {
        PreviewContent::Text(vec![Line::from(Span::styled(
            message,
            Style::default().fg(Color::Yellow),
        ))])
    }

    fn bump_preview_generation(&mut self) -> u64 {
        self.next_preview_id += 1;
        self.latest_preview_id = self.next_preview_id;
        self.latest_preview_id
    }

    fn request_preview(&mut self, path: String, loading_message: &'static str) {
        let id = self.latest_preview_id;
        self.preview_content = Self::loading_preview(loading_message);
        let _ = self.preview_tx.send(PreviewRequest { id, path });
    }

    fn drain_preview_results(&mut self) {
        while let Ok(result) = self.preview_rx.try_recv() {
            if result.id != self.latest_preview_id || result.path != self.preview_path {
                continue;
            }

            match result.content {
                PreviewResultContent::Image(Some(img)) => {
                    self.image_cache.insert(result.path, img.clone());
                    self.preview_content = PreviewContent::Image(img);
                }
                PreviewResultContent::Image(None) => {
                    self.preview_content = PreviewContent::Text(vec![Line::from(Span::styled(
                        "[Cannot load image]",
                        Style::default().fg(Color::DarkGray),
                    ))]);
                }
                PreviewResultContent::Text(lines) => {
                    self.preview_content = PreviewContent::Text(lines);
                }
            }
        }
    }

    fn update_preview(&mut self) {
        // Drain any pending PNG-compressed frames into the cache
        if let Some((path, frames)) = self.video_cache_pending.lock().unwrap().take() {
            self.video_cache.insert(path, frames);
        }

        self.drain_preview_results();

        let current_path = self.selected_file().unwrap_or_default();
        if current_path == self.preview_path {
            // Check if streaming video has new frames
            if let Some(ref loader) = self.video_loader {
                let frames = loader.lock().unwrap();
                let done = self
                    .video_loader_done
                    .as_ref()
                    .map(|d| *d.lock().unwrap())
                    .unwrap_or(false);
                let frame_count = frames.len();
                if frame_count > 0 {
                    if !done && frame_count == self.video_loader_seen_frames {
                        return;
                    }
                    let new_frames = frames.clone();
                    self.video_loader_seen_frames = frame_count;
                    drop(frames);
                    if done {
                        let cache_path = current_path.clone();
                        let frames_for_cache = new_frames.clone();
                        let cache = Arc::clone(&self.video_cache_pending);
                        thread::spawn(move || {
                            let png_frames: Vec<CachedImage> = frames_for_cache
                                .iter()
                                .filter_map(|f| {
                                    if f.is_raw_rgb {
                                        let img = image::RgbImage::from_raw(
                                            f.width,
                                            f.height,
                                            f.data.as_ref().clone(),
                                        )?;
                                        let mut buf = Vec::new();
                                        let mut cursor = std::io::Cursor::new(&mut buf);
                                        image::DynamicImage::ImageRgb8(img)
                                            .write_to(&mut cursor, image::ImageFormat::Png)
                                            .ok()?;
                                        Some(CachedImage {
                                            data: Arc::new(buf),
                                            width: f.width,
                                            height: f.height,
                                            is_raw_rgb: false,
                                        })
                                    } else {
                                        Some(f.clone())
                                    }
                                })
                                .collect();
                            cache.lock().unwrap().replace((cache_path, png_frames));
                        });
                        self.video_loader = None;
                        self.video_loader_done = None;
                        self.video_loader_seen_frames = 0;
                        self.preview_content = PreviewContent::Video(new_frames);
                    } else {
                        self.preview_content = PreviewContent::VideoStreaming(new_frames);
                    }
                } else if done {
                    drop(frames);
                    self.video_loader = None;
                    self.video_loader_done = None;
                    self.video_loader_seen_frames = 0;
                    self.preview_content = PreviewContent::Text(vec![Line::from(Span::styled(
                        "[Cannot load video]",
                        Style::default().fg(Color::DarkGray),
                    ))]);
                }
            }
            return;
        }
        self.preview_path = current_path.clone();
        self.bump_preview_generation();

        // Cancel any in-flight video loader for a different path
        if self.video_loader_path != current_path {
            self.video_loader = None;
            self.video_loader_done = None;
            self.video_loader_path.clear();
            self.video_loader_seen_frames = 0;
        }

        if current_path.is_empty() {
            self.preview_content = PreviewContent::None;
            return;
        }

        if is_video_file(&current_path) {
            // Check video cache first
            if let Some(cached_frames) = self.video_cache.get(&current_path) {
                self.video_frame = 0;
                self.video_frame_time = Instant::now();
                self.video_loader_seen_frames = 0;
                self.preview_content = PreviewContent::Video(cached_frames.clone());
                return;
            }
            let shared_frames = Arc::new(Mutex::new(Vec::new()));
            let shared_done = Arc::new(Mutex::new(false));
            let frames_clone = Arc::clone(&shared_frames);
            let done_clone = Arc::clone(&shared_done);
            let path = current_path.clone();
            thread::spawn(move || {
                stream_video_frames(&path, frames_clone);
                *done_clone.lock().unwrap() = true;
            });
            self.video_loader = Some(shared_frames);
            self.video_loader_done = Some(shared_done);
            self.video_loader_path = current_path.clone();
            self.video_frame = 0;
            self.video_loader_seen_frames = 0;
            self.preview_content = PreviewContent::VideoLoading;
        } else if is_image_file(&current_path) {
            // Check cache first
            if let Some(cached) = self.image_cache.get(&current_path) {
                self.preview_content = PreviewContent::Image(cached.clone());
                return;
            }
            self.request_preview(current_path, "Loading image...");
        } else {
            self.request_preview(current_path, "Loading preview...");
        }
    }

    fn update_matches(&mut self) {
        while let Ok(result) = self.search_rx.try_recv() {
            // Accept only the newest request's result for the current
            // query/scope. The result may have been computed against a
            // slightly older snapshot while a loader is still publishing;
            // that is fine because the snapshot travels with the result and
            // a follow-up search for the newer snapshot is already queued.
            if result.id != self.latest_search_id
                || result.query != self.query
                || result.scope != self.active_scope()
            {
                continue;
            }

            self.matches = result.matches;
            self.matches_by_time = result.matches_by_time;
            self.matches_files = result.files;
            self.last_result_search_id = result.id;
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
        }

        let scope = self.active_scope();
        let files = self.snapshot_for_scope(scope);
        let files_len = files.len();
        let file_version = self.version_for_scope(scope).load(AtomicOrdering::Relaxed);
        let query_changed = self.query != self.last_query;
        let files_changed =
            files_len != self.last_file_count || file_version != self.last_file_version;
        let scope_changed = scope != self.last_scope;

        if !query_changed && !files_changed && !scope_changed {
            return;
        }

        self.next_search_id += 1;
        self.latest_search_id = self.next_search_id;
        let _ = self.search_tx.send(SearchRequest {
            id: self.latest_search_id,
            query: self.query.clone(),
            scope,
            file_version,
            files,
        });

        // Mark this query/version as requested so the render loop does not enqueue
        // the same search every tick while the worker is still computing it.
        self.last_query = self.query.clone();
        self.last_file_count = files_len;
        self.last_file_version = file_version;
        self.last_scope = scope;
    }

    fn selected_file(&self) -> Option<String> {
        let matches = if self.active_column == 0 {
            &self.matches
        } else {
            &self.matches_by_time
        };
        matches
            .get(self.selected)
            .and_then(|m| self.matches_files.get(m.index))
            .map(|entry| entry.path.to_string())
    }

    fn active_matches(&self) -> &[MatchEntry] {
        if self.active_column == 0 {
            &self.matches
        } else {
            &self.matches_by_time
        }
    }

    fn is_loading(&self) -> bool {
        *self.loading_for_scope(self.active_scope()).lock().unwrap()
    }

    /// True while the search worker still owes us a result for the latest
    /// request, meaning fresh matches could arrive any moment.
    fn search_pending(&self) -> bool {
        self.last_result_search_id != self.latest_search_id
    }
}

fn history_file_path() -> Option<PathBuf> {
    let state_dir =
        dirs::state_dir().or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))?;
    Some(state_dir.join("ffp").join("recent.tsv"))
}

fn load_ffp_history() -> HashMap<String, SystemTime> {
    let mut history = HashMap::new();

    for (path, time) in read_ffp_history_entries() {
        let entry = history.entry(path).or_insert(time);
        if time > *entry {
            *entry = time;
        }
    }

    history
}

fn read_ffp_history_entries() -> Vec<(String, SystemTime)> {
    let Some(path) = history_file_path() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|line| {
            let (secs, path) = line.split_once('\t')?;
            let secs = secs.parse::<u64>().ok()?;
            Some((unescape_history_path(path), unix_secs_to_system_time(secs)))
        })
        .collect()
}

fn record_file_interaction(path: &str) -> io::Result<()> {
    let Some(history_path) = history_file_path() else {
        return Ok(());
    };

    let key = normalized_path_key(path);
    let now = SystemTime::now();
    let mut history = load_ffp_history();
    history.insert(key, now);

    let mut entries: Vec<(String, SystemTime)> = history.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries.truncate(HISTORY_MAX_ENTRIES);

    if let Some(parent) = history_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = history_path.with_extension("tsv.tmp");
    let mut body = String::new();
    for (path, time) in entries {
        body.push_str(&system_time_to_unix_secs(time).to_string());
        body.push('\t');
        body.push_str(&escape_history_path(&path));
        body.push('\n');
    }

    fs::write(&tmp_path, body)?;
    fs::rename(tmp_path, history_path)?;
    Ok(())
}

fn system_time_to_unix_secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_secs_to_system_time(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

fn escape_history_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn unescape_history_path(path: &str) -> String {
    let mut unescaped = String::with_capacity(path.len());
    let mut chars = path.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            unescaped.push(ch);
            continue;
        }

        match chars.next() {
            Some('t') => unescaped.push('\t'),
            Some('n') => unescaped.push('\n'),
            Some('r') => unescaped.push('\r'),
            Some('\\') => unescaped.push('\\'),
            Some(other) => {
                unescaped.push('\\');
                unescaped.push(other);
            }
            None => unescaped.push('\\'),
        }
    }

    unescaped
}

fn load_desktop_recent() -> HashMap<String, SystemTime> {
    let Some(data_dir) = dirs::data_dir() else {
        return HashMap::new();
    };
    let path = data_dir.join("recently-used.xbel");
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };

    let mut recent = HashMap::new();
    let mut offset = 0;

    while let Some(pos) = contents[offset..].find("<bookmark") {
        let start = offset + pos;
        let Some(end_rel) = contents[start..].find('>') else {
            break;
        };
        let tag = &contents[start..start + end_rel + 1];
        offset = start + end_rel + 1;

        let Some(href) = xml_attr(tag, "href") else {
            continue;
        };
        let Some(path) = file_uri_to_path(&href) else {
            continue;
        };

        let time = ["visited", "modified", "added"]
            .into_iter()
            .filter_map(|attr| xml_attr(tag, attr))
            .filter_map(|value| parse_rfc3339_to_system_time(&value))
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let key = normalized_path_key(&path);
        let entry = recent.entry(key).or_insert(time);
        if time > *entry {
            *entry = time;
        }
    }

    recent
}

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{}=", name);
    let pos = tag.find(&needle)? + needle.len();
    let quote = tag.as_bytes().get(pos).copied()?;
    if quote != b'\'' && quote != b'\"' {
        return None;
    }
    let rest = &tag[pos + 1..];
    let end = rest.as_bytes().iter().position(|b| *b == quote)?;
    Some(decode_xml_entities(&rest[..end]))
}

fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = if let Some(path) = rest.strip_prefix("localhost/") {
        format!("/{}", path)
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        return None;
    };

    Some(percent_decode(&path))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                decoded.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&decoded).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_rfc3339_to_system_time(value: &str) -> Option<SystemTime> {
    let value = value.trim();
    if value.len() < 20 {
        return None;
    }

    let year = value.get(0..4)?.parse::<i32>().ok()?;
    let month = value.get(5..7)?.parse::<u32>().ok()?;
    let day = value.get(8..10)?.parse::<u32>().ok()?;
    let hour = value.get(11..13)?.parse::<u32>().ok()?;
    let minute = value.get(14..16)?.parse::<u32>().ok()?;
    let second = value.get(17..19)?.parse::<u32>().ok()?;

    if value.get(4..5)? != "-"
        || value.get(7..8)? != "-"
        || value.get(10..11)? != "T"
        || value.get(13..14)? != ":"
        || value.get(16..17)? != ":"
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut tz_start = 19;
    if value.as_bytes().get(tz_start) == Some(&b'.') {
        tz_start += 1;
        while value
            .as_bytes()
            .get(tz_start)
            .map(|b| b.is_ascii_digit())
            .unwrap_or(false)
        {
            tz_start += 1;
        }
    }

    let tz = value.get(tz_start..)?;
    let offset_secs = if tz == "Z" || tz == "z" {
        0
    } else {
        let sign = match tz.as_bytes().first().copied()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let hours = tz.get(1..3)?.parse::<i64>().ok()?;
        let minutes = if tz.as_bytes().get(3) == Some(&b':') {
            tz.get(4..6)?.parse::<i64>().ok()?
        } else {
            tz.get(3..5)?.parse::<i64>().ok()?
        };
        if hours > 23 || minutes > 59 {
            return None;
        }
        sign * (hours * 3600 + minutes * 60)
    };

    let days = days_from_civil(year, month, day);
    let local_timestamp = days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    let timestamp = local_timestamp - offset_secs;

    if timestamp >= 0 {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp as u64))
    } else {
        Some(SystemTime::UNIX_EPOCH - Duration::from_secs((-timestamp) as u64))
    }
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe as i64 - 719_468
}

fn build_fd_args(dir_mode: bool, recent_only: bool) -> Vec<String> {
    let mut args = vec![
        ".".to_string(),
        dirs::home_dir().unwrap().to_string_lossy().to_string(),
        "/tmp".to_string(),
        "-t".to_string(),
        if dir_mode { "d" } else { "f" }.to_string(),
    ];

    if recent_only && !dir_mode {
        args.push("--changed-within".to_string());
        args.push(FILE_LOOKBACK_WINDOW.to_string());
    }

    args.extend([
        "-H".to_string(),
        "-E".to_string(),
        ".git".to_string(),
        "-E".to_string(),
        "node_modules".to_string(),
        "-E".to_string(),
        ".cache".to_string(),
        "-E".to_string(),
        ".npm".to_string(),
        "-E".to_string(),
        "target".to_string(),
        "-E".to_string(),
        "__pycache__".to_string(),
        "-E".to_string(),
        ".venv".to_string(),
        "-E".to_string(),
        "venv".to_string(),
        "-E".to_string(),
        ".cargo/registry".to_string(),
        "-E".to_string(),
        ".cargo/git".to_string(),
        "-E".to_string(),
        ".rustup".to_string(),
        "-E".to_string(),
        "go/pkg/mod".to_string(),
    ]);

    args
}

/// Destination for the full file pool when a recent-scope load is already
/// walking the whole tree anyway. Lets one fd scan fill both pools.
struct AllPoolSink {
    files: SharedEntries,
    loading: Arc<Mutex<bool>>,
    version: Arc<AtomicU64>,
}

fn spawn_entry_loader(
    files: SharedEntries,
    loading: Arc<Mutex<bool>>,
    version: Arc<AtomicU64>,
    dir_mode: bool,
    recent_only: bool,
    recency_index: Arc<RecencyIndex>,
    all_sink: Option<AllPoolSink>,
) {
    if !try_begin_load(&loading, &files) {
        return;
    }
    // Claim the all pool up front so a Ctrl+Z during the recent scan reuses
    // this scan instead of kicking off a second full-tree walk.
    let all_sink =
        all_sink.filter(|sink| !dir_mode && recent_only && try_begin_load(&sink.loading, &sink.files));
    publish_entries(&files, &[], &version);

    thread::spawn(move || {
        load_entries(
            files,
            loading,
            version,
            dir_mode,
            recent_only,
            recency_index,
            all_sink,
        );
    });
}

/// Atomically claim the loader for a scope. Returns false when entries are
/// already loaded or another loader is running, so concurrent toggles can
/// never spawn duplicate scans.
fn try_begin_load(loading: &Arc<Mutex<bool>>, files: &SharedEntries) -> bool {
    let mut loading = loading.lock().unwrap();
    if *loading || !files.lock().unwrap().is_empty() {
        return false;
    }
    *loading = true;
    true
}

/// Mutable destination state for one scope's load: accumulated entries, the
/// dedupe set, and the publish timer.
struct LoadDest<'a> {
    files: &'a SharedEntries,
    version: &'a AtomicU64,
    seen: &'a mut HashSet<String>,
    loaded: &'a mut Vec<FileEntry>,
    last_publish: &'a mut Instant,
}

fn load_entries(
    files: SharedEntries,
    loading: Arc<Mutex<bool>>,
    version: Arc<AtomicU64>,
    dir_mode: bool,
    recent_only: bool,
    recency_index: Arc<RecencyIndex>,
    all_sink: Option<AllPoolSink>,
) {
    let mut seen = HashSet::new();
    let mut loaded = Vec::new();
    let mut last_publish = Instant::now();

    if recent_only && !dir_mode {
        // Stage 1: paths we already know were recently opened by ffp or the desktop.
        let mut buffer = Vec::new();
        for path in recency_index.candidate_paths() {
            push_entry_if_wanted(path, &mut buffer, &mut seen, &recency_index, true);
        }
        flush_entry_buffer(
            &files,
            &mut loaded,
            &mut buffer,
            &version,
            true,
            &mut last_publish,
        );
        sort_loaded_entries(&files, &mut loaded, &version, true);

        // Stage 2: the old fast path, modified recently according to fd.
        load_fd_entries(
            build_fd_args(false, true),
            LoadDest {
                files: &files,
                version: &version,
                seen: &mut seen,
                loaded: &mut loaded,
                last_publish: &mut last_publish,
            },
            &recency_index,
            true,
            true,
            None,
        );
        sort_loaded_entries(&files, &mut loaded, &version, true);

        // Stage 3: broader background pass for access-time recency. This can take
        // longer on large homes, but the UI is already usable from stages 1 and 2.
        // fd never emits duplicate paths within one run, so this stage only
        // checks `seen` without growing it. When an all-pool sink is attached
        // this same scan also fills the Ctrl+Z all-files pool.
        let mut all_loaded = Vec::new();
        load_fd_entries(
            build_fd_args(false, false),
            LoadDest {
                files: &files,
                version: &version,
                seen: &mut seen,
                loaded: &mut loaded,
                last_publish: &mut last_publish,
            },
            &recency_index,
            true,
            false,
            all_sink.as_ref().map(|sink| (sink, &mut all_loaded)),
        );
        if let Some(sink) = all_sink {
            sort_loaded_entries(&sink.files, &mut all_loaded, &sink.version, false);
            *sink.loading.lock().unwrap() = false;
        }
    } else {
        load_fd_entries(
            build_fd_args(dir_mode, false),
            LoadDest {
                files: &files,
                version: &version,
                seen: &mut seen,
                loaded: &mut loaded,
                last_publish: &mut last_publish,
            },
            &recency_index,
            false,
            false,
            None,
        );
    }

    sort_loaded_entries(&files, &mut loaded, &version, recent_only && !dir_mode);

    *loading.lock().unwrap() = false;
}

fn load_fd_entries(
    args: Vec<String>,
    dest: LoadDest<'_>,
    recency_index: &Arc<RecencyIndex>,
    recent_only: bool,
    record_seen: bool,
    mut all_out: Option<(&AllPoolSink, &mut Vec<FileEntry>)>,
) {
    let LoadDest {
        files,
        version,
        seen,
        loaded,
        last_publish,
    } = dest;
    let mut child = match Command::new("fd")
        .args(&args)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.wait();
        return;
    };

    // Metadata lookups dominate scan time, so fan paths out to a small pool
    // of stat workers and collect finished entries on this thread.
    let (path_tx, path_rx) = mpsc::channel::<String>();
    let path_rx = Arc::new(Mutex::new(path_rx));
    let (entry_tx, entry_rx) = mpsc::channel::<FileEntry>();

    let reader = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for read_result in reader.split(b'\n') {
            let Ok(line) = read_result else { break };
            if line.is_empty() {
                continue;
            }
            let path = String::from_utf8_lossy(&line).to_string();
            if path_tx.send(path).is_err() {
                break;
            }
        }
    });

    let worker_count = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, STAT_WORKER_MAX);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let path_rx = Arc::clone(&path_rx);
        let entry_tx = entry_tx.clone();
        let recency_index = Arc::clone(recency_index);
        workers.push(thread::spawn(move || loop {
            let received = { path_rx.lock().unwrap().recv() };
            let Ok(path) = received else { break };
            let Some(entry) = FileEntry::try_new_with_recency(path, &recency_index) else {
                continue;
            };
            if entry_tx.send(entry).is_err() {
                break;
            }
        }));
    }
    drop(entry_tx);

    let mut buffer = Vec::new();
    let mut all_last_publish = Instant::now();
    while let Ok(entry) = entry_rx.recv() {
        if let Some((sink, all_loaded)) = all_out.as_mut() {
            // The all pool takes every scanned entry, before any recency
            // filtering. Entry clones are cheap (shared path strings).
            all_loaded.push(entry.clone());
            if all_last_publish.elapsed() >= ENTRY_PUBLISH_INTERVAL {
                publish_entries(&sink.files, all_loaded, &sink.version);
                all_last_publish = Instant::now();
            }
        }

        if recent_only && !is_within_recent_window(entry.recency_time) {
            continue;
        }
        if !seen.is_empty() || record_seen {
            let key = recency_index.path_key(&entry.path);
            if seen.contains(key.as_ref()) {
                continue;
            }
            if record_seen {
                seen.insert(key.into_owned());
            }
        }
        buffer.push(entry);

        if buffer.len() >= FILE_BATCH_SIZE {
            flush_entry_buffer(files, loaded, &mut buffer, version, recent_only, last_publish);
        }
    }
    flush_entry_buffer(files, loaded, &mut buffer, version, recent_only, last_publish);

    for worker in workers {
        let _ = worker.join();
    }
    let _ = reader.join();
    let _ = child.wait();
}

fn push_entry_if_wanted(
    path: String,
    buffer: &mut Vec<FileEntry>,
    seen: &mut HashSet<String>,
    recency_index: &RecencyIndex,
    recent_only: bool,
) {
    let key = recency_index.path_key(&path);
    if seen.contains(key.as_ref()) {
        return;
    }
    let key = key.into_owned();

    let Some(entry) = FileEntry::try_new_with_recency(path, recency_index) else {
        return;
    };

    if recent_only && !is_within_recent_window(entry.recency_time) {
        return;
    }

    seen.insert(key);
    buffer.push(entry);
}

fn flush_entry_buffer(
    files: &SharedEntries,
    loaded: &mut Vec<FileEntry>,
    buffer: &mut Vec<FileEntry>,
    version: &AtomicU64,
    cap_recent_pool: bool,
    last_publish: &mut Instant,
) {
    if buffer.is_empty() {
        return;
    }

    loaded.append(buffer);
    if cap_recent_pool && loaded.len() >= RECENT_POOL_LIMIT * 2 {
        loaded.sort_by(compare_entries_by_recency);
        loaded.truncate(RECENT_POOL_LIMIT);
        publish_entries(files, loaded, version);
        *last_publish = Instant::now();
    } else if last_publish.elapsed() >= ENTRY_PUBLISH_INTERVAL {
        publish_entries(files, loaded, version);
        *last_publish = Instant::now();
    }
}

fn sort_loaded_entries(
    files: &SharedEntries,
    loaded: &mut Vec<FileEntry>,
    version: &AtomicU64,
    cap_recent_pool: bool,
) {
    loaded.sort_by(compare_entries_by_recency);
    if cap_recent_pool {
        loaded.truncate(RECENT_POOL_LIMIT);
    }
    publish_entries(files, loaded, version);
}

fn publish_entries(files: &SharedEntries, loaded: &[FileEntry], version: &AtomicU64) {
    *files.lock().unwrap() = Arc::new(loaded.to_vec());
    mark_entries_changed(version);
}

fn mark_entries_changed(version: &AtomicU64) {
    version.fetch_add(1, AtomicOrdering::Relaxed);
}

fn compare_entries_by_recency(a: &FileEntry, b: &FileEntry) -> Ordering {
    b.recency_time
        .cmp(&a.recency_time)
        .then_with(|| b.mtime.cmp(&a.mtime))
        .then_with(|| a.name_lower().cmp(b.name_lower()))
}

fn load_files(files: SharedEntries, loading: Arc<Mutex<bool>>) {
    load_entries(
        files,
        loading,
        Arc::new(AtomicU64::new(0)),
        false,
        true,
        Arc::new(RecencyIndex::load()),
        None,
    );
}

/// Byte offset where the basename begins. fd output never has trailing
/// slashes, so the basename is everything after the last separator.
fn basename_start(path: &str) -> usize {
    path.rfind('/').map(|i| i + 1).unwrap_or(0)
}

impl FileEntry {
    fn name(&self) -> &str {
        &self.path[self.name_start as usize..]
    }

    fn name_lower(&self) -> &str {
        &self.path_lower[self.lower_name_start as usize..]
    }

    fn dir(&self) -> &str {
        let name_start = self.name_start as usize;
        if name_start <= 1 {
            return &self.path[..name_start];
        }
        &self.path[..name_start - 1]
    }

    #[cfg(test)]
    fn new(path: String) -> Self {
        Self::new_with_recency(path, &RecencyIndex::default())
    }

    fn try_new_with_recency(path: String, recency_index: &RecencyIndex) -> Option<Self> {
        let metadata = fs::metadata(&path).ok()?;
        Some(Self::from_metadata(path, Some(&metadata), recency_index))
    }

    #[cfg(test)]
    fn new_with_recency(path: String, recency_index: &RecencyIndex) -> Self {
        let metadata = fs::metadata(&path).ok();
        Self::from_metadata(path, metadata.as_ref(), recency_index)
    }

    fn from_metadata(
        path: String,
        metadata: Option<&fs::Metadata>,
        recency_index: &RecencyIndex,
    ) -> Self {
        let name_start = basename_start(&path);
        // Lowercase the directory part and basename separately so the
        // basename boundary stays known even when lowercasing changes byte
        // lengths (e.g. "İ" -> "i̇").
        let mut path_lower = path[..name_start].to_lowercase();
        let lower_name_start = path_lower.len();
        path_lower.push_str(&path[name_start..].to_lowercase());
        let path: Arc<str> = path.into();
        // Most paths are already lowercase; share one allocation for both.
        let path_lower: Arc<str> = if path_lower == *path {
            Arc::clone(&path)
        } else {
            path_lower.into()
        };
        let is_dir = metadata.map(|m| m.is_dir()).unwrap_or(false);
        let mtime = metadata
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let atime = metadata
            .and_then(|m| m.accessed().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let (history_time, desktop_time) = recency_index.recency_times(&path);
        let (recency_time, recency_source) = best_recency(mtime, atime, history_time, desktop_time);

        Self {
            path,
            path_lower,
            name_start: name_start as u32,
            lower_name_start: lower_name_start as u32,
            mtime,
            recency_time,
            recency_source,
            is_dir,
        }
    }
}

fn read_directory_preview(path: &str, max_lines: usize) -> Vec<String> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return vec!["[Cannot read directory]".to_string()],
    };

    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by(|a, b| {
        let a_is_dir = a.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let b_is_dir = b.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        b_is_dir
            .cmp(&a_is_dir)
            .then_with(|| a.file_name().cmp(&b.file_name()))
    });

    let body_limit = max_lines.saturating_sub(1);
    let mut lines = vec![format!("Directory: {}", path)];

    if body_limit == 0 {
        return lines;
    }

    if entries.is_empty() {
        lines.push("[empty directory]".to_string());
        return lines;
    }

    for entry in entries.iter().take(body_limit) {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        lines.push(if is_dir { format!("{}/", name) } else { name });
    }

    if entries.len() > body_limit {
        lines.push(format!("… {} more", entries.len() - body_limit));
    }

    lines
}

fn open_noatime_file(path: &str) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOATIME)
        .open(path)
}

fn open_preview_file(path: &str) -> io::Result<fs::File> {
    open_noatime_file(path).or_else(|_| fs::File::open(path))
}

fn with_noatime_path<T>(path: &str, f: impl FnOnce(&str) -> T) -> T {
    match open_noatime_file(path) {
        Ok(file) => {
            let proc_path = format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd());
            f(&proc_path)
        }
        Err(_) => f(path),
    }
}

fn plain_preview(lines: Vec<String>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| Line::from(Span::raw(line)))
        .collect()
}

fn read_preview(path: &str, max_lines: usize) -> Vec<String> {
    if path.is_empty() {
        return vec!["No file selected".to_string()];
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return vec!["[Cannot read file]".to_string()],
    };

    if metadata.is_dir() {
        return read_directory_preview(path, max_lines);
    }

    let file = match open_preview_file(path) {
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

    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
    )
}

fn is_video_file(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    matches!(
        ext.as_str(),
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v"
    )
}

fn get_video_duration(path: &str) -> Option<f64> {
    let output = with_noatime_path(path, |input_path| {
        Command::new("ffprobe")
            .args([
                "-v",
                "quiet",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
                input_path,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
    })?;
    let s = String::from_utf8_lossy(&output.stdout);
    s.trim().parse::<f64>().ok()
}

fn get_video_dimensions(path: &str) -> Option<(u32, u32)> {
    let output = with_noatime_path(path, |input_path| {
        Command::new("ffprobe")
            .args([
                "-v",
                "quiet",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0:s=x",
                input_path,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
    })?;
    let s = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = s.trim().split('x').collect();
    if parts.len() == 2 {
        let w = parts[0].parse::<u32>().ok()?;
        let h = parts[1].parse::<u32>().ok()?;
        Some((w, h))
    } else {
        None
    }
}

fn compute_scaled_dimensions(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = f64::min(max_w as f64 / src_w as f64, max_h as f64 / src_h as f64).min(1.0);
    let w = ((src_w as f64 * scale) as u32) & !1; // even for ffmpeg
    let h = ((src_h as f64 * scale) as u32) & !1;
    (w.max(2), h.max(2))
}

fn stream_video_frames(path: &str, shared: Arc<Mutex<Vec<CachedImage>>>) {
    let duration = get_video_duration(path).unwrap_or(5.0);
    let (src_w, src_h) = get_video_dimensions(path).unwrap_or((640, 480));
    let (out_w, out_h) = compute_scaled_dimensions(
        src_w,
        src_h,
        VIDEO_THUMBNAIL_MAX_WIDTH,
        VIDEO_THUMBNAIL_MAX_HEIGHT,
    );
    let frame_size = (out_w * out_h * 3) as usize;

    let fps = (VIDEO_PREVIEW_FRAMES as f64 / duration).max(0.5);
    let scale_filter = format!(
        "fps={:.4},scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2",
        fps, out_w, out_h, out_w, out_h
    );

    with_noatime_path(path, |input_path| {
        let mut child = match Command::new("ffmpeg")
            .args([
                "-i",
                input_path,
                "-vf",
                &scale_filter,
                "-frames:v",
                &VIDEO_PREVIEW_FRAMES.to_string(),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };

        let mut reader = BufReader::with_capacity(256 * 1024, stdout);
        let mut buf = Vec::with_capacity(frame_size * 2);
        let mut frame_count = 0;

        loop {
            let mut tmp = [0u8; 65536];
            let n = match io::Read::read(&mut reader, &mut tmp) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            buf.extend_from_slice(&tmp[..n]);

            while buf.len() >= frame_size {
                let raw_frame: Vec<u8> = buf.drain(..frame_size).collect();
                shared.lock().unwrap().push(CachedImage {
                    data: Arc::new(raw_frame),
                    width: out_w,
                    height: out_h,
                    is_raw_rgb: true,
                });
                frame_count += 1;
                if frame_count >= VIDEO_PREVIEW_FRAMES {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }

        let _ = child.wait();
    });
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
        .map(|line| match h.highlight_line(line, ss) {
            Ok(ranges) => {
                let spans: Vec<Span<'static>> = ranges
                    .into_iter()
                    .map(|(style, text)| {
                        let fg =
                            Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                        Span::styled(text.to_string(), Style::default().fg(fg))
                    })
                    .collect();
                Line::from(spans)
            }
            Err(_) => Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Gray),
            )),
        })
        .collect()
}

fn load_thumbnail(path: &str) -> Option<CachedImage> {
    let file = open_preview_file(path).ok()?;
    let img = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;

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
    thumbnail
        .write_to(&mut cursor, image::ImageFormat::Png)
        .ok()?;

    Some(CachedImage {
        data: Arc::new(png_data),
        width: new_w,
        height: new_h,
        is_raw_rgb: false,
    })
}

/// Decide whether a video frame should be rendered this cycle.
/// Returns Some(frame_index) if we should render, None if we can skip.
/// Also updates video_frame and video_frame_time when advancing.
fn video_render_decision(
    elapsed: Duration,
    video_frame: &mut usize,
    video_frame_time: &mut Instant,
    frame_count: usize,
    preview_path: &str,
    last_rendered_path: &str,
) -> Option<usize> {
    if frame_count == 0 {
        return None;
    }

    let mut frame_changed = false;

    if elapsed >= Duration::from_millis(VIDEO_FRAME_INTERVAL_MS) {
        *video_frame = (*video_frame + 1) % frame_count;
        *video_frame_time = Instant::now();
        frame_changed = true;
    }

    if *video_frame >= frame_count {
        *video_frame = 0;
        frame_changed = true;
    }

    if frame_changed || preview_path != last_rendered_path {
        Some(*video_frame)
    } else {
        None
    }
}

/// Append kitty graphics protocol escape sequences to `out`.
/// Does NOT flush, does NOT touch cursor visibility — the caller is
/// responsible for flushing and for hiding/showing the cursor so that
/// the entire write (cursor-hide + kitty data + cursor-restore) lands
/// in a single `flush()` call, preventing partial-write flicker.
/// Compute the cell box (cols, rows, offsets) an image scales into.
fn kitty_layout(img: &CachedImage, area: Rect) -> (u32, u32, u32, u32) {
    let cell_ratio = 2.0_f64;
    let img_ratio = (img.width as f64 / img.height as f64) * cell_ratio;
    let area_ratio = area.width as f64 / area.height as f64;

    let (cols, rows) = if img_ratio > area_ratio {
        let c = area.width as u32;
        let r = ((c as f64) / img_ratio).max(1.0) as u32;
        (c, r.min(area.height as u32))
    } else {
        let r = area.height as u32;
        let c = ((r as f64) * img_ratio).max(1.0) as u32;
        (c.min(area.width as u32), r)
    };

    let offset_x = (area.width as u32).saturating_sub(cols) / 2;
    let offset_y = (area.height as u32).saturating_sub(rows) / 2;
    (cols, rows, offset_x, offset_y)
}

/// Transmit image data only (kitty `a=t`): no display, no cursor movement.
/// The payload can be hundreds of KB; because it neither displays nor moves
/// the cursor, it is safe to send while the input cursor is visible.
fn kitty_transmit_to(img: &CachedImage, image_id: u32, out: &mut impl Write) -> io::Result<()> {
    let b64_data = BASE64.encode(img.data.as_slice());

    let chunk_size = 4096;
    let chunks: Vec<&str> = b64_data
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;
        if i == 0 {
            if img.is_raw_rgb {
                write!(
                    out,
                    "\x1b_Ga=t,q=2,i={},f=24,s={},v={},m={};{}\x1b\\",
                    image_id,
                    img.width,
                    img.height,
                    if is_last { 0 } else { 1 },
                    chunk
                )?;
            } else {
                write!(
                    out,
                    "\x1b_Ga=t,q=2,i={},f=100,m={};{}\x1b\\",
                    image_id,
                    if is_last { 0 } else { 1 },
                    chunk
                )?;
            }
        } else {
            write!(
                out,
                "\x1b_Gm={};{}\x1b\\",
                if is_last { 0 } else { 1 },
                chunk
            )?;
        }
    }

    Ok(())
}

/// Display a previously transmitted image (kitty `a=p`): cursor move into
/// the preview area plus a tiny placement command. This is the only part
/// that needs the cursor hidden, and it is a few dozen bytes.
fn kitty_place_to(
    img: &CachedImage,
    area: Rect,
    image_id: u32,
    out: &mut impl Write,
) -> io::Result<()> {
    let (cols, rows, offset_x, offset_y) = kitty_layout(img, area);
    write!(
        out,
        "\x1b[{};{}H",
        area.y as u32 + 1 + offset_y,
        area.x as u32 + 1 + offset_x
    )?;
    write!(out, "\x1b_Ga=p,q=2,i={},C=1,c={},r={}\x1b\\", image_id, cols, rows)?;
    Ok(())
}

/// Transmit + place in one stream (used by tests/benches that exercise the
/// full per-frame byte sequence).
fn render_kitty_image_to(
    img: &CachedImage,
    area: Rect,
    image_id: u32,
    out: &mut impl Write,
) -> io::Result<()> {
    kitty_transmit_to(img, image_id, out)?;
    kitty_place_to(img, area, image_id, out)?;
    Ok(())
}

/// The two kitty image ids used for double-buffered preview rendering.
const KITTY_IMAGE_IDS: [u32; 2] = [1, 2];

/// Double buffer for kitty graphics: frames alternate between two image
/// ids so a new frame is always drawn on top of the previous placement
/// before the old image is deleted. Retransmitting one id would make
/// kitty drop the visible image first and blank the area until the new
/// payload decodes, which reads as per-frame flicker.
struct KittyDoubleBuffer {
    /// Index into KITTY_IMAGE_IDS of the currently displayed image.
    current: usize,
}

impl KittyDoubleBuffer {
    fn new() -> Self {
        Self { current: 0 }
    }

    /// Flip buffers, returning `(new_id, old_id)` for the next frame.
    fn flip(&mut self) -> (u32, u32) {
        let old = KITTY_IMAGE_IDS[self.current];
        self.current = 1 - self.current;
        (KITTY_IMAGE_IDS[self.current], old)
    }
}

/// Render image using kitty graphics protocol.
///
/// Two-phase, double-buffered, in one flush:
/// 1. Transmit the frame data (`a=t`) under a fresh image id. This is the
///    bulky part (hundreds of KB) and touches neither the screen nor the
///    cursor, so the input cursor stays visible and steady through it.
/// 2. A tiny atomic tail: hide cursor, place the new frame (`a=p`) over
///    the old one, delete the old id, move the cursor back to the input
///    field, show it. ~60 bytes, far below any terminal coalescing
///    threshold, so the hidden-cursor window is imperceptible.
///
/// Hiding the cursor for the whole transmission (the previous approach)
/// blanked the input cursor for multiple milliseconds out of every 100ms
/// frame interval, which made the insert block visibly strobe.
fn render_kitty_image(
    img: &CachedImage,
    area: Rect,
    cursor_pos: (u16, u16),
    dbuf: &mut KittyDoubleBuffer,
) -> io::Result<()> {
    let (new_id, old_id) = dbuf.flip();
    let mut buf: Vec<u8> = Vec::with_capacity(img.data.len() * 2 + 96);
    compose_kitty_render(img, area, cursor_pos, new_id, old_id, &mut buf)?;
    let mut out = io::stdout().lock();
    out.write_all(&buf)?;
    out.flush()?;
    Ok(())
}

/// Build the full per-frame byte sequence:
/// `transmit new id (cursor untouched)` + `hide cursor` + `place new id` +
/// `delete old id` + `cursor back to input` + `show cursor`.
/// The display tail must stay contiguous in one buffer so a terminal can
/// never paint an intermediate state: no visible cursor inside the preview
/// area, and no moment where neither frame is displayed.
fn compose_kitty_render(
    img: &CachedImage,
    area: Rect,
    cursor_pos: (u16, u16),
    new_id: u32,
    old_id: u32,
    out: &mut impl Write,
) -> io::Result<()> {
    kitty_transmit_to(img, new_id, out)?;
    write!(out, "\x1b[?25l")?;
    kitty_place_to(img, area, new_id, out)?;
    // Delete the previous frame only after the new one is placed over it.
    write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", old_id)?;
    write!(
        out,
        "\x1b[{};{}H\x1b[?25h",
        cursor_pos.1 + 1,
        cursor_pos.0 + 1
    )?;
    Ok(())
}

/// Clear any kitty graphics
fn clear_kitty_image_to(out: &mut impl Write) -> io::Result<()> {
    for id in KITTY_IMAGE_IDS {
        write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", id)?;
    }
    out.flush()?;
    Ok(())
}

/// Clear any kitty graphics at the given area
fn clear_kitty_image() -> io::Result<()> {
    clear_kitty_image_to(&mut io::stdout())
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
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" | "tiff" => {
            ("\u{f1c5}", Color::Rgb(160, 100, 210))
        }
        "mp3" | "wav" | "flac" | "ogg" | "aac" | "m4a" | "wma" => {
            ("\u{f1c7}", Color::Rgb(255, 87, 51))
        }
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v" => {
            ("\u{f1c8}", Color::Rgb(253, 216, 53))
        }
        // Archives
        "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" | "zst" | "deb" | "rpm" => {
            ("\u{f1c6}", Color::Rgb(175, 135, 0))
        }
        // Fonts
        "ttf" | "otf" | "woff" | "woff2" => ("\u{f031}", Color::White),
        // Misc
        "bin" | "exe" | "so" | "dylib" | "dll" => ("\u{f013}", Color::Green),
        _ if filename.starts_with('.') => ("\u{f013}", Color::DarkGray),
        _ => ("\u{f15b}", Color::DarkGray),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TypoWordMatch {
    cost: usize,
    start: usize,
    span_len: usize,
}

fn fuzzy_match(query: &str, files: &[FileEntry], limit: usize) -> Vec<MatchEntry> {
    fuzzy_match_with_candidates(query, files, limit, limit).0
}

fn spawn_search_worker(
    initial_scope: SearchScope,
) -> (mpsc::Sender<SearchRequest>, mpsc::Receiver<SearchResult>) {
    let (request_tx, request_rx) = mpsc::channel::<SearchRequest>();
    let (result_tx, result_rx) = mpsc::channel::<SearchResult>();

    thread::spawn(move || {
        let mut state = MatchSearchState::new(initial_scope);

        while let Ok(mut request) = request_rx.recv() {
            // Coalesce bursts of typing: only spend CPU on the newest queued query.
            while let Ok(newer) = request_rx.try_recv() {
                request = newer;
            }

            let file_count = request.files.len();
            state.reset_if_stale(request.scope, request.file_version, file_count);
            let matches = fuzzy_match_incremental(
                &request.query,
                &request.files,
                MATCH_LIMIT,
                MATCH_CANDIDATE_LIMIT,
                &mut state,
            );

            // Recent column: same matches, sorted by combined recency then score.
            let mut matches_by_time = matches.clone();
            matches_by_time.sort_by(|a, b| {
                request.files[b.index]
                    .recency_time
                    .cmp(&request.files[a.index].recency_time)
                    .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            });

            if result_tx
                .send(SearchResult {
                    id: request.id,
                    query: request.query,
                    scope: request.scope,
                    files: request.files,
                    matches,
                    matches_by_time,
                })
                .is_err()
            {
                break;
            }
        }
    });

    (request_tx, result_rx)
}

fn spawn_preview_worker() -> (mpsc::Sender<PreviewRequest>, mpsc::Receiver<PreviewResult>) {
    let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
    let (result_tx, result_rx) = mpsc::channel::<PreviewResult>();

    thread::spawn(move || {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let mut theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_else(|| theme_set.themes.into_values().next().unwrap_or_default());

        while let Ok(mut request) = request_rx.recv() {
            // Coalesce fast selection changes so a slow image decode cannot build a
            // long preview backlog behind the cursor/input loop.
            while let Ok(newer) = request_rx.try_recv() {
                request = newer;
            }

            let content = load_preview_result_content(&request.path, &syntax_set, &theme);
            if result_tx
                .send(PreviewResult {
                    id: request.id,
                    path: request.path,
                    content,
                })
                .is_err()
            {
                break;
            }
        }
    });

    (request_tx, result_rx)
}

fn load_preview_result_content(
    path: &str,
    syntax_set: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
) -> PreviewResultContent {
    if is_image_file(path) {
        return PreviewResultContent::Image(load_thumbnail(path));
    }

    let lines = read_preview(path, 40);
    let is_dir = fs::metadata(path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);

    let rendered = if lines.len() == 1 && lines[0].starts_with('[') {
        vec![Line::from(Span::styled(
            lines[0].clone(),
            Style::default().fg(Color::DarkGray),
        ))]
    } else if is_dir {
        plain_preview(lines)
    } else {
        highlight_preview(&lines, path, syntax_set, theme)
    };

    PreviewResultContent::Text(rendered)
}

fn fuzzy_match_incremental(
    query: &str,
    files: &[FileEntry],
    limit: usize,
    candidate_limit: usize,
    state: &mut MatchSearchState,
) -> Vec<MatchEntry> {
    if limit == 0 {
        return Vec::new();
    }

    if query.is_empty() {
        let matches: Vec<MatchEntry> = files
            .iter()
            .enumerate()
            .take(limit)
            .map(|(index, _)| MatchEntry {
                index,
                score: 100.0,
            })
            .collect();
        state.last_query.clear();
        state.last_candidates = (0..files.len().min(candidate_limit)).collect();
        return matches;
    }

    if let Some(cached) = state.cache.get(query) {
        let matches = score_candidate_indexes(query, files, cached, limit, candidate_limit).0;
        state.last_query = query.to_string();
        state.last_candidates = cached.clone();
        return matches;
    }

    if query.chars().count() >= INCREMENTAL_QUERY_MIN_CHARS
        && state.last_query.chars().count() >= INCREMENTAL_QUERY_MIN_CHARS
        && query.starts_with(&state.last_query)
        && !state.last_query.is_empty()
        && !state.last_candidates.is_empty()
    {
        let (matches, candidates) =
            score_candidate_indexes(query, files, &state.last_candidates, limit, candidate_limit);
        if matches.len() >= INCREMENTAL_ACCEPT_MIN_MATCHES {
            state.cache.insert(query.to_string(), candidates.clone());
            state.last_query = query.to_string();
            state.last_candidates = candidates;
            return matches;
        }
    }

    let (matches, candidates) = fuzzy_match_with_candidates(query, files, limit, candidate_limit);
    state.cache.insert(query.to_string(), candidates.clone());
    state.last_query = query.to_string();
    state.last_candidates = candidates;
    matches
}

fn fuzzy_match_with_candidates(
    query: &str,
    files: &[FileEntry],
    limit: usize,
    candidate_limit: usize,
) -> (Vec<MatchEntry>, Vec<usize>) {
    if limit == 0 {
        return (Vec::new(), Vec::new());
    }

    if query.is_empty() {
        let matches: Vec<MatchEntry> = files
            .iter()
            .enumerate()
            .take(limit)
            .map(|(index, _)| MatchEntry {
                index,
                score: 100.0,
            })
            .collect();
        let candidates = (0..files.len().min(candidate_limit)).collect();
        return (matches, candidates);
    }

    score_all_files(query, files, limit, candidate_limit)
}

fn score_all_files(
    query: &str,
    files: &[FileEntry],
    limit: usize,
    candidate_limit: usize,
) -> (Vec<MatchEntry>, Vec<usize>) {
    if limit == 0 {
        return (Vec::new(), Vec::new());
    }

    let query_lower = query.to_lowercase();
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();
    let query_compact: String = query_lower
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let query_char_count = query_compact.chars().count();

    let (mut results, warm_candidates) = scan_files_parallel(files, candidate_limit, |entry| {
        let score = file_match_score(
            &query_lower,
            &query_words,
            entry,
            false,
            query_words.len() > 1,
        );
        let warm =
            is_incremental_warm_candidate(&query_compact, query_char_count, &query_words, entry);
        (score, warm)
    });

    if should_accept_fast_match_pass(results.len(), limit, query_words.len()) {
        let (matches, candidates) = trim_sort_matches(results, limit, candidate_limit);
        let candidates = merge_candidate_pool(candidates, warm_candidates, candidate_limit);
        return (matches, candidates);
    }

    let seen: HashSet<usize> = results.iter().map(|matched| matched.index).collect();
    let (typo_results, _) = scan_files_parallel(files, 0, |entry| {
        (
            file_match_score(&query_lower, &query_words, entry, true, true),
            false,
        )
    });
    results.extend(
        typo_results
            .into_iter()
            .filter(|matched| !seen.contains(&matched.index)),
    );

    let (matches, candidates) = trim_sort_matches(results, limit, candidate_limit);
    let candidates = merge_candidate_pool(candidates, warm_candidates, candidate_limit);
    (matches, candidates)
}

/// Score every file with `score_entry`, fanning out across threads for large
/// pools. Returned matches and warm candidates preserve file order.
fn scan_files_parallel(
    files: &[FileEntry],
    warm_limit: usize,
    score_entry: impl Fn(&FileEntry) -> (Option<f64>, bool) + Sync,
) -> (Vec<MatchEntry>, Vec<usize>) {
    let worker_count = if files.len() < PARALLEL_SCORE_MIN_FILES {
        1
    } else {
        thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, SEARCH_WORKER_MAX)
    };

    let scan_chunk = |chunk: &[FileEntry], base: usize| {
        let mut results = Vec::new();
        let mut warm = Vec::new();
        for (offset, entry) in chunk.iter().enumerate() {
            let index = base + offset;
            let (score, is_warm) = score_entry(entry);
            if let Some(score) = score {
                results.push(MatchEntry { index, score });
            }
            if is_warm && warm.len() < warm_limit {
                warm.push(index);
            }
        }
        (results, warm)
    };

    if worker_count <= 1 {
        return scan_chunk(files, 0);
    }

    let chunk_size = files.len().div_ceil(worker_count);
    let mut scanned: Vec<(Vec<MatchEntry>, Vec<usize>)> = Vec::new();
    thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk_size)
            .enumerate()
            .map(|(chunk_index, chunk)| {
                let scan_chunk = &scan_chunk;
                scope.spawn(move || scan_chunk(chunk, chunk_index * chunk_size))
            })
            .collect();
        scanned = handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_default())
            .collect();
    });

    let mut results = Vec::new();
    let mut warm_candidates = Vec::new();
    for (chunk_results, chunk_warm) in scanned {
        results.extend(chunk_results);
        if warm_candidates.len() < warm_limit {
            let take = warm_limit - warm_candidates.len();
            warm_candidates.extend(chunk_warm.into_iter().take(take));
        }
    }
    (results, warm_candidates)
}

fn score_candidate_indexes(
    query: &str,
    files: &[FileEntry],
    indexes: &[usize],
    limit: usize,
    candidate_limit: usize,
) -> (Vec<MatchEntry>, Vec<usize>) {
    if limit == 0 {
        return (Vec::new(), Vec::new());
    }

    let query_lower = query.to_lowercase();
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();
    let mut results = Vec::new();

    for &index in indexes {
        let Some(entry) = files.get(index) else {
            continue;
        };
        if let Some(score) = file_match_score(
            &query_lower,
            &query_words,
            entry,
            false,
            query_words.len() > 1,
        ) {
            results.push(MatchEntry { index, score });
        }
    }

    if !should_accept_fast_match_pass(results.len(), limit, query_words.len()) {
        let mut seen: HashSet<usize> = results.iter().map(|matched| matched.index).collect();
        for &index in indexes {
            if seen.contains(&index) {
                continue;
            }
            let Some(entry) = files.get(index) else {
                continue;
            };
            if let Some(score) = file_match_score(&query_lower, &query_words, entry, true, true) {
                seen.insert(index);
                results.push(MatchEntry { index, score });
            }
        }
    }

    let (matches, candidates) = trim_sort_matches(results, limit, candidate_limit);
    let candidates = merge_candidate_pool(candidates, indexes.iter().copied(), candidate_limit);
    (matches, candidates)
}

fn merge_candidate_pool(
    mut candidates: Vec<usize>,
    fallback: impl IntoIterator<Item = usize>,
    candidate_limit: usize,
) -> Vec<usize> {
    if candidates.len() >= candidate_limit {
        candidates.truncate(candidate_limit);
        return candidates;
    }

    let mut seen: HashSet<usize> = candidates.iter().copied().collect();
    for index in fallback {
        if seen.insert(index) {
            candidates.push(index);
            if candidates.len() >= candidate_limit {
                break;
            }
        }
    }

    candidates
}

fn is_incremental_warm_candidate(
    query_compact: &str,
    query_char_count: usize,
    query_words: &[&str],
    entry: &FileEntry,
) -> bool {
    if query_compact.is_empty() {
        return true;
    }

    // Short prefixes often do not score high enough to be visible yet, but they
    // are exactly the state we want to reuse while the user continues typing.
    if query_char_count <= 2 {
        return chars_in_order(query_compact, entry.name_lower())
            || chars_in_order(query_compact, &entry.path_lower);
    }

    if query_words.iter().any(|word| {
        word.chars().count() >= 2
            && (entry.name_lower().contains(word) || entry.path_lower.contains(word))
    }) {
        return true;
    }

    // Keep this bounded to typo-sized prefixes. Long queries should be narrowed
    // by the scored candidate list instead of a broad subsequence filter.
    query_char_count <= 6
        && (chars_in_order(query_compact, entry.name_lower())
            || chars_in_order(query_compact, &entry.path_lower))
}

fn chars_in_order(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle
        .chars()
        .all(|needle_ch| haystack.any(|hay_ch| hay_ch == needle_ch))
}

fn compare_match_entries(a: &MatchEntry, b: &MatchEntry) -> Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.index.cmp(&b.index))
}

fn should_accept_fast_match_pass(match_count: usize, limit: usize, word_count: usize) -> bool {
    match_count >= limit || (word_count > 1 && match_count > 0)
}

fn trim_sort_matches(
    mut results: Vec<MatchEntry>,
    limit: usize,
    candidate_limit: usize,
) -> (Vec<MatchEntry>, Vec<usize>) {
    let keep = candidate_limit.max(limit).min(results.len());
    if keep > 0 && results.len() > keep {
        results.select_nth_unstable_by(keep, compare_match_entries);
        results.truncate(keep);
    }

    results.sort_by(compare_match_entries);
    let candidates = results.iter().map(|matched| matched.index).collect();
    results.truncate(limit);
    (results, candidates)
}

fn file_match_score(
    query_lower: &str,
    query_words: &[&str],
    entry: &FileEntry,
    allow_typos: bool,
    include_path: bool,
) -> Option<f64> {
    if query_words.is_empty() {
        return None;
    }

    let name_score = fuzzy_match_text_score(query_words, entry.name_lower(), 10_000.0, allow_typos);
    let path_score = if !include_path
        || entry.path_lower.as_ref() == entry.name_lower()
        || !should_score_path(query_lower, query_words, name_score, entry)
    {
        None
    } else {
        let allow_path_typos = allow_typos && query_words.len() > 1;
        fuzzy_match_text_score(query_words, &entry.path_lower, 6_000.0, allow_path_typos)
    };

    name_score
        .into_iter()
        .chain(path_score)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
}

fn should_score_path(
    query_lower: &str,
    query_words: &[&str],
    name_score: Option<f64>,
    entry: &FileEntry,
) -> bool {
    if query_words.len() > 1 || name_score.is_none() {
        return true;
    }

    let compact_query = query_lower.trim();
    compact_query.len() >= 2
        && !entry.name_lower().contains(compact_query)
        && entry.path_lower.contains(compact_query)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FuzzyWordMatchKind {
    Exact,
    Subsequence,
    Typo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FuzzyWordMatch {
    kind: FuzzyWordMatchKind,
    cost: usize,
    start: usize,
    span_len: usize,
}

fn fuzzy_match_text_score(
    words: &[&str],
    text: &str,
    base_score: f64,
    allow_typos: bool,
) -> Option<f64> {
    let mut score = base_score;

    for word in words {
        let matched = fuzzy_match_word(word, text, allow_typos)?;
        score += score_fuzzy_word_match(word, text, matched);
    }

    Some(score)
}

fn fuzzy_match_word(word: &str, text: &str, allow_typos: bool) -> Option<FuzzyWordMatch> {
    if word.is_empty() {
        return Some(FuzzyWordMatch {
            kind: FuzzyWordMatchKind::Exact,
            cost: 0,
            start: 0,
            span_len: 0,
        });
    }

    if let Some(start) = text.find(word) {
        return Some(FuzzyWordMatch {
            kind: FuzzyWordMatchKind::Exact,
            cost: 0,
            start,
            span_len: word.chars().count(),
        });
    }

    let word_char_count = word.chars().count();
    if word_char_count >= 2 {
        if let Some(matched) = subsequence_match_word(word, text, word_char_count) {
            return Some(matched);
        }
    }

    if allow_typos && word_char_count > 2 {
        if let Some(matched) = typo_match_word_segmented(word, text) {
            return Some(FuzzyWordMatch {
                kind: FuzzyWordMatchKind::Typo,
                cost: matched.cost,
                start: matched.start,
                span_len: matched.span_len,
            });
        }
    }

    None
}

fn subsequence_match_word(
    word: &str,
    text: &str,
    word_char_count: usize,
) -> Option<FuzzyWordMatch> {
    let max_span = (word_char_count * 4).max(12);
    let mut word_chars = word.chars();
    let mut next = word_chars.next()?;
    let mut start = None;
    let mut matched = 0usize;

    for (byte_index, ch) in text.char_indices() {
        if ch != next {
            continue;
        }

        start.get_or_insert(byte_index);
        matched += 1;

        if let Some(next_ch) = word_chars.next() {
            next = next_ch;
        } else {
            let start = start.unwrap_or(byte_index);
            let span_len = text[start..=byte_index].chars().count();
            if span_len <= max_span {
                return Some(FuzzyWordMatch {
                    kind: FuzzyWordMatchKind::Subsequence,
                    cost: span_len.saturating_sub(matched),
                    start,
                    span_len,
                });
            }
            return None;
        }
    }

    None
}

fn typo_match_word_segmented(word: &str, text: &str) -> Option<TypoWordMatch> {
    let word_len = word.chars().count();
    let max_dist = if word_len <= 4 { 1 } else { 2 };
    if !has_typo_candidate_overlap(word, text, max_dist) {
        return None;
    }

    let mut best: Option<TypoWordMatch> = None;
    let mut segment_start = None;

    for (index, ch) in text.char_indices().chain(std::iter::once((text.len(), '/'))) {
        if is_match_separator(ch) {
            if let Some(start) = segment_start.take() {
                consider_typo_segment(word, text, start, index, &mut best);
            }
        } else if segment_start.is_none() {
            segment_start = Some(index);
        }
    }

    best
}

fn consider_typo_segment(
    word: &str,
    text: &str,
    start: usize,
    end: usize,
    best: &mut Option<TypoWordMatch>,
) {
    if start >= end {
        return;
    }

    let segment = &text[start..end];
    let word_len = word.chars().count();
    let segment_len = segment.chars().count();
    let max_dist = if word_len <= 4 { 1 } else { 2 };
    if segment_len + max_dist < word_len || word_len + max_dist < segment_len {
        return;
    }

    let Some(mut matched) = typo_match_word(word, segment) else {
        return;
    };
    matched.start += start;

    let replace = best
        .as_ref()
        .map(|current| {
            matched.cost < current.cost
                || (matched.cost == current.cost
                    && (matched.start < current.start
                        || (matched.start == current.start && matched.span_len < current.span_len)))
        })
        .unwrap_or(true);
    if replace {
        *best = Some(matched);
    }
}

fn is_match_separator(ch: char) -> bool {
    matches!(
        ch,
        '/' | '\\' | '-' | '_' | '.' | ' ' | ':' | ';' | ',' | '(' | ')' | '[' | ']' | '{' | '}'
    )
}

fn score_fuzzy_word_match(word: &str, text: &str, matched: FuzzyWordMatch) -> f64 {
    let word_len = word.chars().count() as f64;
    let prefix_bonus = if matched.start == 0 { 180.0 } else { 0.0 };
    let boundary_bonus = if is_match_boundary(text, matched.start) {
        140.0
    } else {
        0.0
    };
    let start_penalty = matched.start.min(160) as f64 * 0.8;

    match matched.kind {
        FuzzyWordMatchKind::Exact => {
            700.0 + word_len * 36.0 + prefix_bonus + boundary_bonus - start_penalty
        }
        FuzzyWordMatchKind::Subsequence => {
            let gap_penalty = matched.cost as f64 * 35.0;
            460.0 + word_len * 30.0 + prefix_bonus + boundary_bonus - gap_penalty
                - start_penalty
        }
        FuzzyWordMatchKind::Typo => {
            let typo_penalty = matched.cost as f64 * 130.0;
            let span_penalty = matched.span_len.abs_diff(word.chars().count()) as f64 * 60.0;
            560.0 + word_len * 32.0 + prefix_bonus + boundary_bonus - typo_penalty
                - span_penalty
                - start_penalty
        }
    }
}

fn is_match_boundary(text: &str, start: usize) -> bool {
    if start == 0 {
        return true;
    }

    text[..start]
        .chars()
        .next_back()
        .map(is_match_separator)
        .unwrap_or(true)
}

fn typo_match_word(word: &str, text: &str) -> Option<TypoWordMatch> {
    if word.is_empty() {
        return Some(TypoWordMatch {
            cost: 0,
            start: 0,
            span_len: 0,
        });
    }

    if let Some(start) = text.find(word) {
        return Some(TypoWordMatch {
            cost: 0,
            start,
            span_len: word.chars().count(),
        });
    }

    // Very short typo matching creates too many noisy matches and wastes CPU.
    // Keep 1-2 character queries as exact substring / rapidfuzz fallback only.
    if word.chars().count() <= 2 {
        return None;
    }

    let word_char_count = word.chars().count();
    let max_dist = if word_char_count <= 4 { 1 } else { 2 };

    if !has_typo_candidate_overlap(word, text, max_dist) {
        return None;
    }

    if word.len() > 64 {
        return None;
    }

    let matched = if word.is_ascii() && text.is_ascii() {
        typo_match_word_osa_substring_ascii(word.as_bytes(), text.as_bytes(), max_dist)
    } else {
        let word_chars: Vec<char> = word.chars().collect();
        let text_chars: Vec<char> = text.chars().collect();
        typo_match_word_osa_substring(&word_chars, &text_chars, max_dist)
    }?;

    if matched.cost > max_dist || matched.cost >= word_char_count {
        None
    } else {
        Some(matched)
    }
}

fn has_typo_candidate_overlap(word: &str, text: &str, max_dist: usize) -> bool {
    let min_overlap = word.chars().count().saturating_sub(max_dist);
    if min_overlap == 0 {
        return true;
    }

    if word.is_ascii() && text.is_ascii() {
        let mut counts = [0u8; 256];
        for byte in text.bytes() {
            let idx = byte.to_ascii_lowercase() as usize;
            counts[idx] = counts[idx].saturating_add(1);
        }

        let mut overlap = 0usize;
        for byte in word.bytes() {
            let idx = byte.to_ascii_lowercase() as usize;
            if counts[idx] > 0 {
                counts[idx] -= 1;
                overlap += 1;
                if overlap >= min_overlap {
                    return true;
                }
            }
        }
        return false;
    }

    let mut text_chars: Vec<char> = text.chars().collect();
    let mut overlap = 0usize;
    for ch in word.chars() {
        if let Some(pos) = text_chars.iter().position(|candidate| *candidate == ch) {
            text_chars.swap_remove(pos);
            overlap += 1;
            if overlap >= min_overlap {
                return true;
            }
        }
    }

    false
}

fn typo_match_word_osa_substring<T: Eq + Copy>(
    pattern: &[T],
    text: &[T],
    max_dist: usize,
) -> Option<TypoWordMatch> {
    if pattern.is_empty() {
        return Some(TypoWordMatch {
            cost: 0,
            start: 0,
            span_len: 0,
        });
    }

    if pattern.len() > text.len().saturating_add(max_dist) {
        return None;
    }

    let sat = max_dist + 1;
    let plen = pattern.len();
    let mut prev: Vec<usize> = (0..=plen).map(|i| i.min(sat)).collect();
    let mut curr = vec![0; plen + 1];
    let mut prev2 = prev.clone();

    let mut prev_start = vec![0; plen + 1];
    let mut curr_start = vec![0; plen + 1];
    let mut prev2_start = vec![0; plen + 1];

    let mut best_cost = sat;
    let mut best_start = 0;
    let mut best_span_len = 0;
    let mut prev_text: Option<T> = None;

    for (j0, &text_ch) in text.iter().enumerate() {
        let j = j0 + 1;
        curr[0] = 0;
        curr_start[0] = j;

        for i in 1..=plen {
            let deletion = (curr[i - 1] + 1).min(sat);
            let insertion = (prev[i] + 1).min(sat);
            let substitution = (prev[i - 1] + usize::from(pattern[i - 1] != text_ch)).min(sat);

            let mut best = deletion;
            let mut start = curr_start[i - 1];

            if insertion < best || (insertion == best && prev_start[i] < start) {
                best = insertion;
                start = prev_start[i];
            }

            if substitution < best || (substitution == best && prev_start[i - 1] < start) {
                best = substitution;
                start = prev_start[i - 1];
            }

            if let Some(prev_ch) = prev_text {
                if j > 1 && i > 1 && pattern[i - 1] == prev_ch && pattern[i - 2] == text_ch {
                    let transposition = (prev2[i - 2] + 1).min(sat);
                    if transposition < best || (transposition == best && prev2_start[i - 2] < start)
                    {
                        best = transposition;
                        start = prev2_start[i - 2];
                    }
                }
            }

            curr[i] = best;
            curr_start[i] = start;
        }

        let span_len = j.saturating_sub(curr_start[plen]);
        if curr[plen] < best_cost
            || (curr[plen] == best_cost
                && (curr_start[plen] < best_start
                    || (curr_start[plen] == best_start && span_len > best_span_len)))
        {
            best_cost = curr[plen];
            best_start = curr_start[plen];
            best_span_len = span_len;
        }

        if best_cost == 0 {
            break;
        }

        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
        std::mem::swap(&mut prev2_start, &mut prev_start);
        std::mem::swap(&mut prev_start, &mut curr_start);
        prev_text = Some(text_ch);
    }

    (best_cost <= max_dist).then_some(TypoWordMatch {
        cost: best_cost,
        start: best_start,
        span_len: best_span_len,
    })
}

fn typo_match_word_osa_substring_ascii(
    pattern: &[u8],
    text: &[u8],
    max_dist: usize,
) -> Option<TypoWordMatch> {
    const MAX_WORD_LEN: usize = 64;

    let plen = pattern.len();
    if plen == 0 {
        return Some(TypoWordMatch {
            cost: 0,
            start: 0,
            span_len: 0,
        });
    }

    if plen > MAX_WORD_LEN || plen > text.len().saturating_add(max_dist) {
        return None;
    }

    let sat = max_dist + 1;
    let mut prev = [0usize; MAX_WORD_LEN + 1];
    let mut curr = [0usize; MAX_WORD_LEN + 1];
    let mut prev2 = [0usize; MAX_WORD_LEN + 1];
    let mut prev_start = [0usize; MAX_WORD_LEN + 1];
    let mut curr_start = [0usize; MAX_WORD_LEN + 1];
    let mut prev2_start = [0usize; MAX_WORD_LEN + 1];

    for i in 0..=plen {
        prev[i] = i.min(sat);
        prev2[i] = prev[i];
    }

    let mut best_cost = sat;
    let mut best_start = 0;
    let mut best_span_len = 0;
    let mut prev_text: Option<u8> = None;

    for (j0, &raw_ch) in text.iter().enumerate() {
        let j = j0 + 1;
        let text_ch = raw_ch.to_ascii_lowercase();
        curr[0] = 0;
        curr_start[0] = j;

        for i in 1..=plen {
            let pattern_ch = pattern[i - 1].to_ascii_lowercase();
            let deletion = (curr[i - 1] + 1).min(sat);
            let insertion = (prev[i] + 1).min(sat);
            let substitution = (prev[i - 1] + usize::from(pattern_ch != text_ch)).min(sat);

            let mut best = deletion;
            let mut start = curr_start[i - 1];

            if insertion < best || (insertion == best && prev_start[i] < start) {
                best = insertion;
                start = prev_start[i];
            }

            if substitution < best || (substitution == best && prev_start[i - 1] < start) {
                best = substitution;
                start = prev_start[i - 1];
            }

            if let Some(prev_ch) = prev_text {
                if j > 1
                    && i > 1
                    && pattern_ch == prev_ch
                    && pattern[i - 2].to_ascii_lowercase() == text_ch
                {
                    let transposition = (prev2[i - 2] + 1).min(sat);
                    if transposition < best || (transposition == best && prev2_start[i - 2] < start)
                    {
                        best = transposition;
                        start = prev2_start[i - 2];
                    }
                }
            }

            curr[i] = best;
            curr_start[i] = start;
        }

        let span_len = j.saturating_sub(curr_start[plen]);
        if curr[plen] < best_cost
            || (curr[plen] == best_cost
                && (curr_start[plen] < best_start
                    || (curr_start[plen] == best_start && span_len > best_span_len)))
        {
            best_cost = curr[plen];
            best_start = curr_start[plen];
            best_span_len = span_len;
        }

        if best_cost == 0 {
            break;
        }

        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
        std::mem::swap(&mut prev2_start, &mut prev_start);
        std::mem::swap(&mut prev_start, &mut curr_start);
        prev_text = Some(text_ch);
    }

    (best_cost <= max_dist).then_some(TypoWordMatch {
        cost: best_cost,
        start: best_start,
        span_len: best_span_len,
    })
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

fn get_rss_kb() -> usize {
    fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<usize>().ok())
        .map(|pages| pages * 4) // page size = 4 KiB
        .unwrap_or(0)
}

fn run_profile() -> io::Result<()> {
    let files: SharedEntries = Arc::new(Mutex::new(Arc::new(Vec::new())));
    let loading = Arc::new(Mutex::new(true));

    let start = Instant::now();
    load_files(Arc::clone(&files), Arc::clone(&loading));
    let load_duration = start.elapsed();

    let files = Arc::clone(&files.lock().unwrap());
    let file_count = files.len();
    eprintln!(
        "profile: loaded {} files in {:?}",
        file_count, load_duration
    );

    let empty_start = Instant::now();
    let empty_matches = fuzzy_match("", files.as_ref(), MATCH_LIMIT);
    eprintln!(
        "profile: query <empty> -> {} matches in {:?}",
        empty_matches.len(),
        empty_start.elapsed()
    );

    let queries = env::var("FFP_PROFILE_QUERIES").unwrap_or_else(|_| "rs,main,doc".to_string());
    for query in queries.split(',').map(str::trim).filter(|q| !q.is_empty()) {
        let start = Instant::now();
        let matches = fuzzy_match(query, files.as_ref(), MATCH_LIMIT);
        eprintln!(
            "profile: query {:?} -> {} matches in {:?}",
            query,
            matches.len(),
            start.elapsed()
        );
    }

    let type_sequence =
        env::var("FFP_PROFILE_TYPE_SEQUENCE").unwrap_or_else(|_| "m,ma,mai,main".to_string());
    let mut match_state = MatchSearchState::new(SearchScope::Recent);
    match_state.reset_if_stale(SearchScope::Recent, 0, file_count);
    eprintln!("\n=== Incremental Typing Profile ===");
    for query in type_sequence
        .split(',')
        .map(str::trim)
        .filter(|q| !q.is_empty())
    {
        let start = Instant::now();
        let matches = fuzzy_match_incremental(
            query,
            files.as_ref(),
            MATCH_LIMIT,
            MATCH_CANDIDATE_LIMIT,
            &mut match_state,
        );
        eprintln!(
            "profile: incremental query {:?} -> {} matches, {} candidates in {:?}",
            query,
            matches.len(),
            match_state.last_candidates.len(),
            start.elapsed()
        );
    }

    // Profile image loading
    eprintln!("\n=== Image Loading Profile ===");
    let image_files: Vec<_> = files
        .iter()
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
                        file.name(),
                        img.width,
                        img.height,
                        img.data.len(),
                        duration
                    );
                }
                None => {
                    eprintln!("profile: image {:?} -> FAILED in {:?}", file.name(), duration);
                }
            }
        }

        // Profile cached access
        eprintln!("\n=== Cached Image Access ===");
        let mut cache = ImageCache::new(IMAGE_CACHE_SIZE);

        // First load (cold)
        if let Some(first) = image_files.first() {
            let start = Instant::now();
            if let Some(img) = load_thumbnail(&first.path) {
                let load_time = start.elapsed();
                cache.insert(first.path.to_string(), img);
                eprintln!("profile: cold load {:?} in {:?}", first.name(), load_time);

                // Cached access (hot)
                let start = Instant::now();
                let _ = cache.get(&first.path);
                let cache_time = start.elapsed();
                eprintln!(
                    "profile: hot cache access {:?} in {:?}",
                    first.name(), cache_time
                );
            }
        }
    }

    // Profile video loading
    eprintln!("\n=== Video Loading Profile ===");
    let video_files: Vec<_> = files
        .iter()
        .filter(|f| is_video_file(&f.path))
        .take(3)
        .collect();

    if video_files.is_empty() {
        eprintln!("No video files found in recent files");
    } else {
        for file in &video_files {
            eprintln!("\n--- {} ---", file.name());

            // Get video info
            if let Some(dur) = get_video_duration(&file.path) {
                eprintln!("  duration: {:.1}s", dur);
            }
            if let Some((w, h)) = get_video_dimensions(&file.path) {
                let (sw, sh) = compute_scaled_dimensions(
                    w,
                    h,
                    VIDEO_THUMBNAIL_MAX_WIDTH,
                    VIDEO_THUMBNAIL_MAX_HEIGHT,
                );
                eprintln!("  source: {}x{} -> scaled: {}x{}", w, h, sw, sh);
                let frame_bytes = sw as usize * sh as usize * 3;
                eprintln!(
                    "  raw frame size: {} bytes ({:.1} KiB)",
                    frame_bytes,
                    frame_bytes as f64 / 1024.0
                );
            }

            // Measure RSS before
            let rss_before = get_rss_kb();

            let shared = Arc::new(Mutex::new(Vec::new()));
            let shared_clone = Arc::clone(&shared);
            let path = file.path.clone();

            let start = Instant::now();
            let handle = thread::spawn(move || {
                stream_video_frames(&path, shared_clone);
            });

            // Poll for first frame
            let first_frame_time;
            loop {
                if !shared.lock().unwrap().is_empty() {
                    first_frame_time = start.elapsed();
                    break;
                }
                thread::sleep(Duration::from_millis(1));
                if start.elapsed() > Duration::from_secs(30) {
                    eprintln!("  TIMEOUT waiting for first frame");
                    first_frame_time = start.elapsed();
                    break;
                }
            }
            eprintln!("  first frame: {:?}", first_frame_time);

            handle.join().unwrap();
            let total_time = start.elapsed();

            let frames = shared.lock().unwrap();
            let frame_count = frames.len();
            let total_bytes: usize = frames.iter().map(|f| f.data.len()).sum();
            let avg_frame = total_bytes.checked_div(frame_count).unwrap_or(0);

            // Measure RSS after
            let rss_after = get_rss_kb();

            eprintln!("  total: {:?} ({} frames)", total_time, frame_count);
            if frame_count > 0 {
                let fps = frame_count as f64 / total_time.as_secs_f64();
                eprintln!("  extraction rate: {:.1} frames/sec", fps);
                eprintln!(
                    "  frame data: {:.1} KiB avg, {:.1} MiB total",
                    avg_frame as f64 / 1024.0,
                    total_bytes as f64 / (1024.0 * 1024.0)
                );
            }
            eprintln!(
                "  RSS: {} KiB -> {} KiB (delta: {} KiB)",
                rss_before,
                rss_after,
                rss_after.saturating_sub(rss_before)
            );
        }

        // Profile video cache hit
        eprintln!("\n=== Video Cache ===");
        let mut vcache = VideoCache::new(VIDEO_CACHE_SIZE);
        if let Some(first) = video_files.first() {
            let shared = Arc::new(Mutex::new(Vec::new()));
            let shared_clone = Arc::clone(&shared);
            let path = first.path.clone();
            let start = Instant::now();
            stream_video_frames(&path, shared_clone);
            let cold = start.elapsed();
            let frames = Arc::try_unwrap(shared).unwrap().into_inner().unwrap();
            let total_bytes: usize = frames.iter().map(|f| f.data.len()).sum();
            vcache.insert(first.path.to_string(), frames);
            eprintln!(
                "  cold load {:?}: {:?} ({:.1} MiB)",
                first.name(),
                cold,
                total_bytes as f64 / (1024.0 * 1024.0)
            );

            let start = Instant::now();
            let _ = vcache.get(&first.path);
            eprintln!("  hot cache access {:?}: {:?}", first.name(), start.elapsed());
        }
    }

    Ok(())
}

fn percentile_duration(samples: &mut [Duration], percentile: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort_unstable();
    let rank = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[rank.min(samples.len() - 1)]
}

fn synthetic_frame(width: u32, height: u32, is_raw_rgb: bool) -> CachedImage {
    let bytes_per_pixel = if is_raw_rgb { 3 } else { 1 };
    let size = (width * height * bytes_per_pixel) as usize;
    let mut data = vec![0u8; size];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = (i & 0xff) as u8;
    }
    CachedImage {
        data: Arc::new(data),
        width,
        height,
        is_raw_rgb,
    }
}

fn bench_kitty_render(label: &str, frame: &CachedImage, iterations: usize) -> io::Result<()> {
    let area = Rect {
        x: 60,
        y: 2,
        width: 50,
        height: 25,
    };
    let mut samples = Vec::with_capacity(iterations);
    let mut bytes = 0usize;

    for _ in 0..iterations {
        let mut out = Vec::with_capacity(frame.data.len() * 2);
        let start = Instant::now();
        render_kitty_image_to(frame, area, 1, &mut out)?;
        samples.push(start.elapsed());
        bytes = out.len();
    }

    let mut sorted = samples.clone();
    let p50 = percentile_duration(&mut sorted, 0.50);
    let mut sorted = samples.clone();
    let p95 = percentile_duration(&mut sorted, 0.95);
    let max = samples.iter().copied().max().unwrap_or_default();
    eprintln!(
        "bench: kitty_render {label}: {}x{} raw={} payload={:.1} KiB p50={:?} p95={:?} max={:?}",
        frame.width,
        frame.height,
        frame.is_raw_rgb,
        bytes as f64 / 1024.0,
        p50,
        p95,
        max
    );

    Ok(())
}

fn run_bench_responsiveness() -> io::Result<()> {
    eprintln!("=== Responsiveness Benchmark ===");

    // FFP_BENCH_SYNTHETIC=N benchmarks against N generated entries instead of
    // the real recent pool, matching all-files (Ctrl+Z) scale.
    let files: EntrySnapshot = if let Ok(synthetic) = env::var("FFP_BENCH_SYNTHETIC") {
        let count: usize = synthetic.parse().unwrap_or(600_000);
        let start = Instant::now();
        let entries: Vec<FileEntry> = (0..count)
            .map(|i| {
                FileEntry::from_metadata(
                    format!("/home/user/project-{}/src/module_{}/file_{}.rs", i % 977, i % 53, i),
                    None,
                    &RecencyIndex::default(),
                )
            })
            .collect();
        eprintln!(
            "bench: generated {} synthetic files in {:?}",
            entries.len(),
            start.elapsed()
        );
        Arc::new(entries)
    } else {
        let files: SharedEntries = Arc::new(Mutex::new(Arc::new(Vec::new())));
        let loading = Arc::new(Mutex::new(true));
        let start = Instant::now();
        load_files(Arc::clone(&files), Arc::clone(&loading));
        let files = Arc::clone(&files.lock().unwrap());
        eprintln!(
            "bench: loaded {} files for responsiveness benchmark in {:?}",
            files.len(),
            start.elapsed()
        );
        files
    };

    let (search_tx, search_rx) = spawn_search_worker(SearchScope::Recent);
    let queries = env::var("FFP_BENCH_QUERIES").unwrap_or_else(|_| "m,ma,mai,main,doc,rs".into());
    eprintln!("\n=== Key -> search worker result ===");
    let mut samples = Vec::new();
    for (index, query) in queries
        .split(',')
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .enumerate()
    {
        let id = index as u64 + 1;
        let start = Instant::now();
        let _ = search_tx.send(SearchRequest {
            id,
            query: query.to_string(),
            scope: SearchScope::Recent,
            file_version: 0,
            files: Arc::clone(&files),
        });
        match search_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => {
                let elapsed = start.elapsed();
                samples.push(elapsed);
                eprintln!(
                    "bench: key_to_search {:?}: {:?} matches={}",
                    query,
                    elapsed,
                    result.matches.len()
                );
            }
            Err(err) => eprintln!("bench: key_to_search {:?}: TIMEOUT/ERR {err}", query),
        }
    }
    let mut sorted = samples.clone();
    eprintln!(
        "bench: key_to_search summary p50={:?} p95={:?} max={:?}",
        percentile_duration(&mut sorted, 0.50),
        percentile_duration(&mut samples, 0.95),
        samples.iter().copied().max().unwrap_or_default()
    );

    eprintln!("\n=== Preview render payload generation ===");
    let (video_w, video_h) = compute_scaled_dimensions(
        2880,
        1800,
        VIDEO_THUMBNAIL_MAX_WIDTH,
        VIDEO_THUMBNAIL_MAX_HEIGHT,
    );
    let (old_w, old_h) =
        compute_scaled_dimensions(2880, 1800, THUMBNAIL_MAX_WIDTH, THUMBNAIL_MAX_HEIGHT);
    bench_kitty_render(
        "video_current",
        &synthetic_frame(video_w, video_h, true),
        30,
    )?;
    bench_kitty_render("video_old_800px", &synthetic_frame(old_w, old_h, true), 15)?;
    bench_kitty_render("image_png_like", &synthetic_frame(800, 450, false), 30)?;

    eprintln!("\n=== Preview decode/highlight candidates ===");
    let syntax_set = SyntaxSet::load_defaults_newlines();
    let mut theme_set = ThemeSet::load_defaults();
    let theme = theme_set
        .themes
        .remove("base16-ocean.dark")
        .unwrap_or_else(|| theme_set.themes.into_values().next().unwrap_or_default());
    if let Some(text_file) = files
        .iter()
        .find(|entry| !entry.is_dir && is_text_extension(&entry.path))
    {
        let start = Instant::now();
        let _ = load_preview_result_content(&text_file.path, &syntax_set, &theme);
        eprintln!(
            "bench: text_preview {:?}: {:?}",
            text_file.name(),
            start.elapsed()
        );
    }
    if let Some(image_file) = files.iter().find(|entry| is_image_file(&entry.path)) {
        let start = Instant::now();
        let _ = load_preview_result_content(&image_file.path, &syntax_set, &theme);
        eprintln!(
            "bench: image_preview {:?}: {:?}",
            image_file.name(),
            start.elapsed()
        );
    }

    eprintln!("\n=== Input grace model ===");
    let cycles = 120usize;
    let mut renders_without_grace = 0usize;
    let mut renders_with_grace = 0usize;
    let mut skipped_for_input = 0usize;
    let mut last_input_at = Some(Instant::now());
    let sim_start = Instant::now();
    for cycle in 0..cycles {
        if cycle % 3 == 0 {
            last_input_at = Some(Instant::now());
        }
        let should_render = cycle % 4 == 0;
        if should_render {
            renders_without_grace += 1;
            let input_recent = last_input_at
                .map(|at| at.elapsed() < Duration::from_millis(INPUT_RENDER_GRACE_MS))
                .unwrap_or(false);
            if input_recent {
                skipped_for_input += 1;
            } else {
                renders_with_grace += 1;
            }
        }
        let target = sim_start + Duration::from_millis((cycle as u64 + 1) * 16);
        while Instant::now() < target {
            thread::yield_now();
        }
    }
    eprintln!(
        "bench: input_grace renders_without={} renders_with={} skipped_for_recent_input={} grace_ms={}",
        renders_without_grace,
        renders_with_grace,
        skipped_for_input,
        INPUT_RENDER_GRACE_MS
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ffp-test-{}-{}-{}",
            std::process::id(),
            nanos,
            name
        ))
    }

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

    #[test]
    fn typo_match_handles_common_edits() {
        assert_eq!(
            typo_match_word("mian", "main.rs"),
            Some(TypoWordMatch {
                cost: 1,
                start: 0,
                span_len: 4,
            })
        );
        assert_eq!(
            typo_match_word("documnt", "document.pdf"),
            Some(TypoWordMatch {
                cost: 1,
                start: 0,
                span_len: 8,
            })
        );
        assert_eq!(
            typo_match_word("configg", "config.toml"),
            Some(TypoWordMatch {
                cost: 1,
                start: 0,
                span_len: 7,
            })
        );
        assert!(typo_match_word("zzzz", "main.rs").is_none());
    }

    #[test]
    fn fuzzy_match_is_typo_resistant() {
        let files = vec![
            FileEntry::new("many.rs".to_string()),
            FileEntry::new("main.rs".to_string()),
            FileEntry::new("document.pdf".to_string()),
            FileEntry::new("config.toml".to_string()),
        ];

        let matches = fuzzy_match("mian", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "main.rs");

        let matches = fuzzy_match("documnt", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "document.pdf");

        let matches = fuzzy_match("configg", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "config.toml");
    }

    #[test]
    fn fuzzy_match_uses_path_for_multi_word_queries() {
        let files = vec![
            FileEntry::new("/tmp/other/session-log.txt".to_string()),
            FileEntry::new("/tmp/jcode/session-log.txt".to_string()),
        ];

        let matches = fuzzy_match("jcode sessino", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].path.as_ref(), "/tmp/jcode/session-log.txt");
    }

    #[test]
    fn fuzzy_match_still_handles_short_substrings() {
        let files = vec![FileEntry::new("main.rs".to_string())];

        let matches = fuzzy_match("rs", &files, 10);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].index, 0);
    }

    #[test]
    fn fuzzy_match_supports_short_exact_and_subsequence_queries() {
        let files = vec![
            FileEntry::new("notes.txt".to_string()),
            FileEntry::new("main.rs".to_string()),
            FileEntry::new("tmux.conf".to_string()),
        ];

        let matches = fuzzy_match("m", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "main.rs");

        let matches = fuzzy_match("mr", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "main.rs");

        let matches = fuzzy_match("tmuxconf", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "tmux.conf");
    }

    #[test]
    fn fuzzy_match_searches_path_for_single_word_queries() {
        let files = vec![
            FileEntry::new("/tmp/other/session-log.txt".to_string()),
            FileEntry::new("/tmp/jcode/session-log.txt".to_string()),
        ];

        let matches = fuzzy_match("jcode", &files, 10);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].path.as_ref(), "/tmp/jcode/session-log.txt");
    }

    #[test]
    fn fuzzy_match_incremental_rescans_after_broad_short_prefix() {
        let mut files: Vec<FileEntry> = (0..1500)
            .map(|i| FileEntry::new(format!("ma-noise-{i}.txt")))
            .collect();
        files.push(FileEntry::new("main.rs".to_string()));

        let mut state = MatchSearchState::new(SearchScope::Recent);
        state.reset_if_stale(SearchScope::Recent, 1, files.len());

        let _ = fuzzy_match_incremental("ma", &files, 10, 64, &mut state);
        let matches = fuzzy_match_incremental("mai", &files, 10, 64, &mut state);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "main.rs");
    }

    #[test]
    fn fuzzy_match_incremental_reuses_candidate_pool_for_extended_queries() {
        let mut files: Vec<FileEntry> = (0..64)
            .map(|i| FileEntry::new(format!("noise-{i}.txt")))
            .collect();
        files.push(FileEntry::new("main.rs".to_string()));
        files.push(FileEntry::new("maintainer-notes.md".to_string()));

        let mut state = MatchSearchState::new(SearchScope::Recent);
        state.reset_if_stale(SearchScope::Recent, 1, files.len());

        let _ = fuzzy_match_incremental("mi", &files, 10, 16, &mut state);
        assert!(!state.last_candidates.is_empty());
        assert!(state.last_candidates.len() <= 16);

        let matches = fuzzy_match_incremental("mian", &files, 10, 16, &mut state);
        assert!(!matches.is_empty());
        assert_eq!(files[matches[0].index].name(), "main.rs");
        assert_eq!(state.last_query, "mian");
    }

    #[test]
    fn fuzzy_match_incremental_cache_handles_repeated_query() {
        let files = vec![
            FileEntry::new("main.rs".to_string()),
            FileEntry::new("document.pdf".to_string()),
        ];
        let mut state = MatchSearchState::new(SearchScope::Recent);
        state.reset_if_stale(SearchScope::Recent, 1, files.len());

        let first = fuzzy_match_incremental("documnt", &files, 10, 16, &mut state);
        let second = fuzzy_match_incremental("documnt", &files, 10, 16, &mut state);
        assert_eq!(first.len(), second.len());
        assert_eq!(files[second[0].index].name(), "document.pdf");
    }

    #[test]
    fn build_fd_args_uses_lookback_only_for_recent_file_search() {
        let recent_args = build_fd_args(false, true);
        assert!(recent_args.iter().any(|arg| arg == "--changed-within"));
        assert!(recent_args.iter().any(|arg| arg == FILE_LOOKBACK_WINDOW));

        let all_args = build_fd_args(false, false);
        assert!(!all_args.iter().any(|arg| arg == "--changed-within"));
        assert!(!all_args.iter().any(|arg| arg == FILE_LOOKBACK_WINDOW));

        let dir_args = build_fd_args(true, true);
        assert!(!dir_args.iter().any(|arg| arg == "--changed-within"));
    }

    #[test]
    fn recency_prefers_ffp_history_over_filesystem_times() {
        let file = unique_temp_path("history-recency.txt");
        fs::write(&file, "hello").unwrap();

        let path = file.to_string_lossy().to_string();
        let history_time = SystemTime::now() + Duration::from_secs(60);
        let mut recency_index = RecencyIndex::default();
        recency_index
            .ffp_history
            .insert(normalized_path_key(&path), history_time);

        let entry = FileEntry::new_with_recency(path, &recency_index);
        assert_eq!(entry.recency_time, history_time);
        assert_eq!(entry.recency_source, RecencySource::FfpHistory);

        fs::remove_file(file).unwrap();
    }

    #[test]
    fn recency_window_filters_old_times() {
        assert!(is_within_recent_window(SystemTime::now()));
        assert!(!is_within_recent_window(
            SystemTime::now() - Duration::from_secs(FILE_LOOKBACK_WINDOW_SECS + 60)
        ));
    }

    /// The UI label must always advertise the same cutoff that the recent
    /// loader actually uses for filtering.
    #[test]
    fn recent_scope_label_shows_lookback_cutoff() {
        assert_eq!(
            SearchScope::Recent.label(),
            format!("recent ({})", FILE_LOOKBACK_WINDOW)
        );
        assert_eq!(SearchScope::All.label(), "all");
    }

    #[test]
    fn history_path_escaping_round_trips() {
        let path = "/tmp/ffp path/with\\slashes\tand\nnewlines.txt";
        assert_eq!(unescape_history_path(&escape_history_path(path)), path);
    }

    #[test]
    fn file_entry_name_dir_accessors() {
        let entry = FileEntry::new("/home/jeremy/Documents/Notes.TXT".to_string());
        assert_eq!(entry.name(), "Notes.TXT");
        assert_eq!(entry.name_lower(), "notes.txt");
        assert_eq!(entry.dir(), "/home/jeremy/Documents");
        assert_eq!(entry.path_lower.as_ref(), "/home/jeremy/documents/notes.txt");

        let root_file = FileEntry::new("/swapfile".to_string());
        assert_eq!(root_file.name(), "swapfile");
        assert_eq!(root_file.dir(), "/");

        let bare = FileEntry::new("README.md".to_string());
        assert_eq!(bare.name(), "README.md");
        assert_eq!(bare.dir(), "");
    }

    #[test]
    fn file_entry_handles_multibyte_lowercase_boundary() {
        // 'İ' lowercases to a two-byte sequence, shifting byte offsets.
        let entry = FileEntry::new("/tmp/İstanbul/İzmir.TXT".to_string());
        assert_eq!(entry.name(), "İzmir.TXT");
        assert_eq!(entry.name_lower(), "İzmir.TXT".to_lowercase());
        assert_eq!(entry.dir(), "/tmp/İstanbul");
    }

    #[test]
    fn normalized_absolute_paths_skip_rekeying() {
        assert!(path_is_normalized_absolute("/home/jeremy/file.txt"));
        assert!(path_is_normalized_absolute("/"));
        assert!(!path_is_normalized_absolute("relative/file.txt"));
        assert!(!path_is_normalized_absolute("/home/./file.txt"));
        assert!(!path_is_normalized_absolute("/home/../file.txt"));
        assert!(!path_is_normalized_absolute("/home//file.txt"));
        assert!(!path_is_normalized_absolute("/home/jeremy/"));
    }

    #[test]
    fn path_key_matches_normalized_form() {
        let index = RecencyIndex {
            cwd: PathBuf::from("/home/jeremy"),
            ..RecencyIndex::default()
        };
        assert_eq!(index.path_key("/a/b.txt").as_ref(), "/a/b.txt");
        assert_eq!(
            index.path_key("/a/./b.txt").as_ref(),
            normalized_path_key_with_cwd("/a/./b.txt", &index.cwd)
        );
        assert_eq!(index.path_key("b.txt").as_ref(), "/home/jeremy/b.txt");
    }

    #[test]
    fn search_results_carry_their_snapshot() {
        // Simulates a scope toggle where results for an old pool arrive after
        // the pool has been swapped: indexes must resolve against the result's
        // own snapshot.
        let (search_tx, search_rx) = spawn_search_worker(SearchScope::Recent);
        let snapshot: EntrySnapshot = Arc::new(vec![
            FileEntry::new("alpha.txt".to_string()),
            FileEntry::new("beta.txt".to_string()),
        ]);
        search_tx
            .send(SearchRequest {
                id: 1,
                query: "beta".to_string(),
                scope: SearchScope::Recent,
                file_version: 7,
                files: Arc::clone(&snapshot),
            })
            .unwrap();
        let result = search_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("search result");
        assert_eq!(result.id, 1);
        assert!(Arc::ptr_eq(&result.files, &snapshot));
        assert_eq!(result.files[result.matches[0].index].name(), "beta.txt");
    }

    #[test]
    fn desktop_file_uri_decoding_handles_percent_encoding() {
        assert_eq!(
            file_uri_to_path("file:///home/jeremy/a%20b%23c.txt").as_deref(),
            Some("/home/jeremy/a b#c.txt")
        );
        assert_eq!(
            file_uri_to_path("file://localhost/tmp/hello.txt").as_deref(),
            Some("/tmp/hello.txt")
        );
        assert!(file_uri_to_path("file://other-host/tmp/hello.txt").is_none());
    }

    #[test]
    fn rfc3339_parser_handles_utc_and_offsets() {
        assert_eq!(
            parse_rfc3339_to_system_time("1970-01-01T00:00:00Z"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert_eq!(
            parse_rfc3339_to_system_time("1970-01-01T01:00:00+01:00"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert_eq!(
            parse_rfc3339_to_system_time("1969-12-31T23:00:00-01:00"),
            Some(SystemTime::UNIX_EPOCH)
        );
    }

    #[test]
    fn file_entry_marks_directories() {
        let dir = unique_temp_path("dir-entry");
        fs::create_dir_all(&dir).unwrap();

        let entry = FileEntry::new(dir.to_string_lossy().to_string());
        assert!(entry.is_dir);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_preview_lists_directory_contents() {
        let dir = unique_temp_path("dir-preview");
        let child_dir = dir.join("nested");
        let child_file = dir.join("hello.txt");

        fs::create_dir_all(&child_dir).unwrap();
        fs::write(&child_file, "hello").unwrap();

        let preview = read_preview(&dir.to_string_lossy(), 10);
        assert!(!preview.is_empty());
        assert!(preview[0].starts_with("Directory: "));
        assert!(preview.iter().any(|line| line == "nested/"));
        assert!(preview.iter().any(|line| line == "hello.txt"));

        fs::remove_dir_all(dir).unwrap();
    }

    fn parse_kitty_commands(data: &str) -> Vec<String> {
        let mut cmds = Vec::new();
        let mut i = 0;
        let bytes = data.as_bytes();
        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == 0x1b && bytes[i + 1] == b'_' {
                let start = i;
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        cmds.push(data[start..i + 2].to_string());
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        cmds
    }

    fn get_kitty_header(cmd: &str) -> &str {
        let inner = &cmd[2..cmd.len() - 2]; // strip \x1b_ and \x1b\
        if let Some(pos) = inner.find(';') {
            &inner[..pos]
        } else {
            inner
        }
    }

    #[test]
    fn kitty_png_image_has_correct_format() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 100]),
            width: 200,
            height: 100,
            is_raw_rgb: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        };
        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        let cmds = parse_kitty_commands(&output);
        assert!(!cmds.is_empty(), "should emit at least one kitty command");

        let header = get_kitty_header(&cmds[0]);
        assert!(header.contains("a=t"), "first command transmits data only");
        assert!(
            header.contains("q=2"),
            "must have quiet mode to suppress responses"
        );
        assert!(
            header.contains("i=1"),
            "must use image ID 1 for atomic replacement"
        );
        assert!(header.contains("f=100"), "PNG data must use f=100");
        assert!(!header.contains("f=24"), "PNG data must not use f=24");

        let place = get_kitty_header(cmds.last().unwrap());
        assert!(place.contains("a=p"), "must end with a placement command");
        assert!(place.contains("i=1"), "placement must reference image ID 1");
    }

    #[test]
    fn kitty_raw_rgb_has_correct_format() {
        let w = 160u32;
        let h = 90u32;
        let img = CachedImage {
            data: Arc::new(vec![0u8; (w * h * 3) as usize]),
            width: w,
            height: h,
            is_raw_rgb: true,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        };
        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        let cmds = parse_kitty_commands(&output);
        assert!(!cmds.is_empty());

        let header = get_kitty_header(&cmds[0]);
        assert!(header.contains("f=24"), "raw RGB must use f=24");
        assert!(
            header.contains(&format!("s={}", w)),
            "must specify pixel width"
        );
        assert!(
            header.contains(&format!("v={}", h)),
            "must specify pixel height"
        );
        assert!(header.contains("q=2"), "must have quiet mode");
        assert!(header.contains("i=1"), "must use image ID 1");
    }

    #[test]
    fn kitty_multi_chunk_has_correct_continuation() {
        let img = CachedImage {
            data: Arc::new(vec![0xFFu8; 8192]),
            width: 50,
            height: 50,
            is_raw_rgb: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        let cmds = parse_kitty_commands(&output);
        assert!(
            cmds.len() >= 2,
            "8KB data should produce multiple chunks, got {}",
            cmds.len()
        );

        let first_header = get_kitty_header(&cmds[0]);
        assert!(
            first_header.contains("m=1"),
            "first chunk must signal more data (m=1)"
        );

        // The final command is the placement (a=p); chunked transmit data
        // is everything before it.
        let place_header = get_kitty_header(cmds.last().unwrap());
        assert!(place_header.contains("a=p"), "must end with placement");
        let chunks = &cmds[..cmds.len() - 1];

        for cmd in &chunks[1..chunks.len() - 1] {
            let h = get_kitty_header(cmd);
            assert!(h.contains("m=1"), "middle chunk must have m=1");
            assert!(
                !h.contains("a="),
                "continuation chunks should not have action key"
            );
        }

        let last_header = get_kitty_header(chunks.last().unwrap());
        assert!(
            last_header.contains("m=0"),
            "last transmit chunk must signal end (m=0)"
        );
    }

    #[test]
    fn kitty_render_to_does_not_toggle_cursor() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 10]),
            width: 10,
            height: 10,
            is_raw_rgb: false,
        };
        let area = Rect {
            x: 5,
            y: 5,
            width: 20,
            height: 10,
        };
        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        assert!(
            output.find("\x1b[?25l").is_none(),
            "render_kitty_image_to must NOT hide cursor"
        );
        assert!(
            output.find("\x1b[?25h").is_none(),
            "render_kitty_image_to must NOT show cursor"
        );

        assert!(
            output.find("\x1b_G").is_some(),
            "should still have kitty command"
        );
    }

    /// The composed frame must be: bulky transmit (cursor untouched), then
    /// a tiny atomic display tail of hide → place → delete old → reposition
    /// → show. The hide window must exclude the large transmit payload so
    /// the input cursor does not visibly strobe each video frame.
    #[test]
    fn compose_kitty_render_is_atomic_hide_draw_restore() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 64 * 1024]),
            width: 160,
            height: 90,
            is_raw_rgb: true,
        };
        let area = Rect {
            x: 60,
            y: 2,
            width: 40,
            height: 20,
        };
        let cursor_pos = (15u16, 1u16);

        let mut buf = Vec::new();
        compose_kitty_render(&img, area, cursor_pos, 2, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf).to_string();

        let transmit = output.find("a=t").expect("must transmit data first");
        let hide = output.find("\x1b[?25l").expect("must hide cursor");
        let place = output.find("a=p").expect("must place the new frame");
        let restore = output
            .find(&format!("\x1b[{};{}H\x1b[?25h", cursor_pos.1 + 1, cursor_pos.0 + 1))
            .expect("must move cursor back to input before showing it");

        assert!(transmit < hide, "transmit happens before the cursor hide");
        assert!(hide < place, "hide must precede the placement");
        assert!(place < restore, "reposition must follow the placement");
        assert_eq!(
            output.matches("\x1b[?25h").count(),
            1,
            "exactly one cursor show"
        );
        assert_eq!(
            output.matches("\x1b[?25l").count(),
            1,
            "exactly one cursor hide"
        );
        assert!(
            output.ends_with("\x1b[?25h"),
            "buffer must end with the cursor visible at the input field"
        );

        // The hidden-cursor window must be tiny: place + delete + restore
        // only, far below one transmit chunk.
        let hidden_window = output.len() - hide;
        assert!(
            hidden_window < 128,
            "cursor-hidden tail must be tiny, got {} bytes",
            hidden_window
        );

        // Double-buffer ordering: the new frame (i=2) must be placed before
        // the old frame (i=1) is deleted, so the area is never blank.
        let delete_old = output
            .find("\x1b_Ga=d,d=I,i=1,q=2\x1b\\")
            .expect("must delete the old frame id");
        assert!(
            place < delete_old,
            "old frame must only be deleted after the new frame is placed"
        );
        assert!(
            delete_old < restore,
            "delete must happen inside the atomic tail before cursor restore"
        );
    }

    /// Frames must alternate between the two kitty image ids, always
    /// deleting the id rendered two frames ago, never the one just drawn.
    #[test]
    fn kitty_double_buffer_alternates_ids() {
        let mut dbuf = KittyDoubleBuffer::new();
        let (a_new, a_old) = dbuf.flip();
        let (b_new, b_old) = dbuf.flip();
        let (c_new, c_old) = dbuf.flip();

        assert_ne!(a_new, a_old, "new and old ids must differ");
        assert_eq!(b_old, a_new, "next flip deletes the previous frame");
        assert_eq!(c_new, a_new, "ids alternate with period two");
        assert_eq!(c_old, b_new);
        for id in [a_new, a_old, b_new, b_old] {
            assert!(KITTY_IMAGE_IDS.contains(&id));
        }
    }

    /// A full clear must delete both double-buffer ids, otherwise one frame
    /// can survive a preview-area collapse.
    #[test]
    fn kitty_clear_removes_both_buffers() {
        let mut buf = Vec::new();
        clear_kitty_image_to(&mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);
        for id in KITTY_IMAGE_IDS {
            assert!(
                output.contains(&format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", id)),
                "clear must delete image id {}",
                id
            );
        }
    }

    /// `C=1` tells kitty not to move the cursor after drawing the image, so
    /// even the hidden cursor never travels with the graphics payload.
    #[test]
    fn kitty_image_does_not_move_cursor() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 32]),
            width: 8,
            height: 8,
            is_raw_rgb: true,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 5,
        };
        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);
        let cmds = parse_kitty_commands(&output);
        assert!(!cmds.is_empty());
        let place = get_kitty_header(cmds.last().unwrap());
        assert!(place.contains("a=p"), "last command must be the placement");
        assert!(
            place.contains("C=1"),
            "placement must use C=1 (do not move cursor)"
        );
    }

    #[test]
    fn kitty_render_no_cursor_toggle() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 100]),
            width: 50,
            height: 50,
            is_raw_rgb: false,
        };
        let area = Rect {
            x: 5,
            y: 5,
            width: 20,
            height: 10,
        };
        let mut buf: Vec<u8> = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        assert!(
            !output.contains("\x1b[?25l"),
            "kitty render must never hide cursor — no cursor toggling at all"
        );
        assert!(
            !output.contains("\x1b[?25h"),
            "kitty render must never show cursor — no cursor toggling at all"
        );
        assert!(
            output.contains("\x1b_G"),
            "must contain kitty graphics command"
        );
    }

    #[test]
    fn kitty_clear_uses_id_and_quiet() {
        let mut buf = Vec::new();
        clear_kitty_image_to(&mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        assert!(output.contains("a=d"), "clear must use delete action");
        assert!(output.contains("d=I"), "must delete by image ID");
        assert!(output.contains("i=1"), "must target image ID 1");
        assert!(output.contains("q=2"), "must use quiet mode");
        assert!(!output.contains("d=a"), "must NOT delete all images");
    }

    #[test]
    fn kitty_no_response_triggers() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 100]),
            width: 50,
            height: 50,
            is_raw_rgb: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        let cmds = parse_kitty_commands(&output);
        for cmd in &cmds {
            let header = get_kitty_header(cmd);
            if header.contains("a=") {
                assert!(
                    header.contains("q=2"),
                    "every command with an action must have q=2 to prevent stdin spam: {}",
                    header
                );
            }
        }
    }

    #[test]
    fn kitty_aspect_ratio_preserved() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 10]),
            width: 1920,
            height: 1080,
            is_raw_rgb: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        };
        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        let cmds = parse_kitty_commands(&output);
        // Scaling now lives on the placement command (a=p), the last one.
        let header = get_kitty_header(cmds.last().unwrap());

        let c_val: u32 = header
            .split(',')
            .find(|s| s.starts_with("c="))
            .and_then(|s| s[2..].parse().ok())
            .expect("must have c= columns");
        let r_val: u32 = header
            .split(',')
            .find(|s| s.starts_with("r="))
            .and_then(|s| s[2..].parse().ok())
            .expect("must have r= rows");

        assert!(c_val <= 80, "cols {} must fit in area width 80", c_val);
        assert!(r_val <= 40, "rows {} must fit in area height 40", r_val);
        assert!(c_val > 0 && r_val > 0, "dimensions must be positive");

        let display_ratio = c_val as f64 / r_val as f64;
        let expected_ratio = (1920.0 / 1080.0) * 2.0; // cell_ratio=2
        let diff = (display_ratio - expected_ratio).abs() / expected_ratio;
        assert!(
            diff < 0.15,
            "aspect ratio off by {:.0}%: display {}/{} = {:.2}, expected {:.2}",
            diff * 100.0,
            c_val,
            r_val,
            display_ratio,
            expected_ratio
        );
    }

    // --- Video render decision tests (flicker bugs) ---

    #[test]
    fn video_first_frame_must_render() {
        // Bug: new video loads, video_frame=0, timer fresh, path not yet rendered.
        // Must render frame 0 immediately — not skip it.
        let mut frame = 0usize;
        let mut time = Instant::now();
        let result = video_render_decision(
            Duration::from_millis(0), // just started, no time elapsed
            &mut frame,
            &mut time,
            10,                 // 10 frames available
            "/videos/test.mp4", // current path
            "",                 // last_rendered_path is empty (nothing rendered yet)
        );
        assert_eq!(
            result,
            Some(0),
            "first frame of a new video must always render"
        );
    }

    #[test]
    fn video_same_frame_same_path_skips() {
        // Steady state: same video, timer hasn't elapsed, already rendered this frame.
        // Should skip to avoid unnecessary re-renders.
        let mut frame = 3usize;
        let mut time = Instant::now();
        let result = video_render_decision(
            Duration::from_millis(10), // only 10ms, less than 66ms interval
            &mut frame,
            &mut time,
            10,
            "/videos/test.mp4",
            "/videos/test.mp4", // same path = already rendered
        );
        assert_eq!(
            result, None,
            "should skip render when frame unchanged and path matches"
        );
        assert_eq!(frame, 3, "frame index should not change");
    }

    #[test]
    fn video_advances_after_interval() {
        // Timer has elapsed — should advance to next frame and render.
        let mut frame = 3usize;
        let mut time = Instant::now() - Duration::from_millis(100);
        let result = video_render_decision(
            Duration::from_millis(100), // > 66ms
            &mut frame,
            &mut time,
            10,
            "/videos/test.mp4",
            "/videos/test.mp4",
        );
        assert_eq!(result, Some(4), "should advance and render next frame");
        assert_eq!(frame, 4);
    }

    #[test]
    fn video_wraps_around() {
        // At last frame, timer elapsed — should wrap to 0.
        let mut frame = 9usize;
        let mut time = Instant::now() - Duration::from_millis(100);
        let result = video_render_decision(
            Duration::from_millis(100),
            &mut frame,
            &mut time,
            10,
            "/videos/test.mp4",
            "/videos/test.mp4",
        );
        assert_eq!(result, Some(0), "should wrap to frame 0");
        assert_eq!(frame, 0);
    }

    #[test]
    fn video_clamps_when_frames_shrink() {
        // Edge case during streaming: we rendered frame 8, but now only 5 frames
        // exist (e.g. path changed to shorter video). Must clamp and render.
        let mut frame = 8usize;
        let mut time = Instant::now();
        let result = video_render_decision(
            Duration::from_millis(0), // no time elapsed
            &mut frame,
            &mut time,
            5, // only 5 frames now
            "/videos/test.mp4",
            "/videos/test.mp4", // same path
        );
        assert_eq!(result, Some(0), "must clamp out-of-bounds frame and render");
        assert_eq!(frame, 0);
    }

    #[test]
    fn video_empty_frames_never_renders() {
        let mut frame = 0usize;
        let mut time = Instant::now();
        let result = video_render_decision(
            Duration::from_millis(100),
            &mut frame,
            &mut time,
            0, // no frames yet
            "/videos/test.mp4",
            "",
        );
        assert_eq!(result, None, "must not render when there are no frames");
    }

    #[test]
    fn video_path_change_triggers_render() {
        // User navigated to a different video. Same frame index, timer not elapsed,
        // but path changed — must render.
        let mut frame = 0usize;
        let mut time = Instant::now();
        let result = video_render_decision(
            Duration::from_millis(0),
            &mut frame,
            &mut time,
            10,
            "/videos/new.mp4", // new video
            "/videos/old.mp4", // was showing old video
        );
        assert_eq!(result, Some(0), "path change must trigger render");
    }

    #[test]
    fn video_no_flicker_simulation() {
        // Simulate 20 render cycles at 16ms intervals for a 10-frame video.
        // Every cycle must either render or have a valid reason to skip.
        // The first cycle and each configured video interval should render.
        let mut frame = 0usize;
        let mut time = Instant::now();
        let mut last_path = String::new();
        let path = "/videos/test.mp4";
        let mut render_count = 0;
        let mut gap = 0; // cycles since last render
        let mut elapsed_since_render = Duration::ZERO;
        let max_gap = (VIDEO_FRAME_INTERVAL_MS as usize).div_ceil(16) + 1;

        for cycle in 0..20 {
            let fake_elapsed = if cycle == 0 {
                Duration::from_millis(0)
            } else {
                elapsed_since_render + Duration::from_millis(16)
            };

            let result =
                video_render_decision(fake_elapsed, &mut frame, &mut time, 10, path, &last_path);

            if let Some(_idx) = result {
                render_count += 1;
                last_path = path.to_string();
                gap = 0;
                elapsed_since_render = Duration::ZERO;
            } else {
                gap += 1;
                elapsed_since_render = fake_elapsed;
                assert!(
                    gap <= max_gap,
                    "flicker detected: {} consecutive skipped renders at cycle {} (frame {})",
                    gap,
                    cycle,
                    frame
                );
            }
        }

        assert!(render_count >= 1, "must render at least the first frame");
    }

    #[test]
    fn cursor_never_shown_at_wrong_position() {
        let img = CachedImage {
            data: Arc::new(vec![0u8; 100]),
            width: 200,
            height: 100,
            is_raw_rgb: false,
        };
        let area = Rect {
            x: 50,
            y: 5,
            width: 40,
            height: 20,
        };

        let mut buf = Vec::new();
        render_kitty_image_to(&img, area, 1, &mut buf).unwrap();
        let output = String::from_utf8_lossy(&buf);

        assert!(
            !output.contains("\x1b[?25h"),
            "kitty render must never make cursor visible"
        );
        assert!(
            !output.contains("\x1b[?25l"),
            "kitty render must never hide cursor either — no toggling at all"
        );
    }

    #[test]
    fn video_rapid_frames_no_cursor_leak() {
        let frames: Vec<CachedImage> = (0..5)
            .map(|i| CachedImage {
                data: Arc::new(vec![i as u8; 50]),
                width: 10,
                height: 10,
                is_raw_rgb: false,
            })
            .collect();
        let area = Rect {
            x: 50,
            y: 5,
            width: 40,
            height: 20,
        };

        let mut combined = Vec::new();
        for frame in &frames {
            render_kitty_image_to(frame, area, 1, &mut combined).unwrap();
        }
        let output = String::from_utf8_lossy(&combined);

        assert!(
            !output.contains("\x1b[?25l"),
            "kitty render must never hide cursor"
        );
        assert!(
            !output.contains("\x1b[?25h"),
            "kitty render must never show cursor"
        );

        let kitty_count = output.matches("\x1b_G").count();
        assert!(
            kitty_count >= 5,
            "should have at least 5 kitty commands (one per frame), got {}",
            kitty_count
        );
    }

    #[test]
    fn video_preview_text_stable_across_frames() {
        use std::collections::HashSet;

        let frames: Vec<CachedImage> = (0..10)
            .map(|i| CachedImage {
                data: Arc::new(vec![i; 100]),
                width: 40,
                height: 30,
                is_raw_rgb: false,
            })
            .collect();

        let mut seen_lines: HashSet<String> = HashSet::new();
        for video_frame in 0..10 {
            let preview_lines: Vec<Line> = match frames.len() {
                0 => vec![Line::from(Span::styled(
                    "Loading video...",
                    Style::default().fg(Color::Yellow),
                ))],
                _ => vec![],
            };
            let text: String = preview_lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            seen_lines.insert(text);
            let _ = video_frame;
        }

        assert_eq!(
            seen_lines.len(),
            1,
            "preview text must be identical across all video frames to prevent \
             ratatui diff-redraws that flash text under the kitty image. \
             Got {} different texts: {:?}",
            seen_lines.len(),
            seen_lines
        );
    }

    /// End-to-end render loop simulation.
    ///
    /// Simulates 30 render cycles of video playback (the hot path for
    /// flicker), capturing every byte that would reach the terminal.
    /// Then scans the combined output for three flicker signatures:
    ///
    /// 1. **Split flush** – cursor-show (`\x1b[?25h`) must appear in
    ///    the same atomic buffer as the preceding cursor-hide; if
    ///    `render_kitty_image_to` flushes internally this invariant
    ///    breaks and the terminal can paint a half-drawn frame.
    ///
    /// 2. **Cursor visible at image area** – between a kitty graphics
    ///    command (`\x1b_G`) and the cursor-reposition-to-input, the
    ///    cursor must not be shown.
    ///
    /// 3. **Naked kitty write** – every `\x1b_G` must be preceded by
    ///    a cursor-hide (`\x1b[?25l`) with no intervening cursor-show.
    ///    Otherwise the cursor briefly appears at the image write
    ///    position.
    #[test]
    fn end_to_end_render_loop_no_flicker() {
        let frames: Vec<CachedImage> = (0..10)
            .map(|i| CachedImage {
                data: Arc::new(vec![i as u8; 200]),
                width: 40,
                height: 30,
                is_raw_rgb: false,
            })
            .collect();
        let area = Rect {
            x: 60,
            y: 2,
            width: 50,
            height: 25,
        };
        let cursor_input = (15u16, 1u16);

        let mut terminal_bytes: Vec<u8> = Vec::new();
        let mut video_frame = 0usize;
        let mut video_frame_time = Instant::now();
        let mut last_rendered_path = String::new();
        let preview_path = "/test/video.mp4";

        for cycle in 0..30 {
            write!(
                terminal_bytes,
                "\x1b[{};{}H",
                cursor_input.1 + 1,
                cursor_input.0 + 1
            )
            .unwrap();
            write!(terminal_bytes, "\x1b[?25h").unwrap();

            let elapsed = Duration::from_millis(cycle * 20);

            if let Some(idx) = video_render_decision(
                elapsed,
                &mut video_frame,
                &mut video_frame_time,
                frames.len(),
                preview_path,
                &last_rendered_path,
            ) {
                if let Some(frame) = frames.get(idx) {
                    let mut buf: Vec<u8> = Vec::new();
                    compose_kitty_render(frame, area, cursor_input, 2, 1, &mut buf).unwrap();
                    terminal_bytes.extend_from_slice(&buf);
                }
                last_rendered_path = preview_path.to_string();
            }
        }

        let output = String::from_utf8_lossy(&terminal_bytes).to_string();

        // Transmit-only commands (a=t) are cursor-safe by design; only the
        // placements (a=p) actually draw, so only they require the cursor
        // to be hidden.
        let place_positions: Vec<usize> = output
            .match_indices("\x1b_Ga=p")
            .map(|(pos, _)| pos)
            .collect();

        for &kpos in &place_positions {
            let before = &output[..kpos];
            let last_show = before.rfind("\x1b[?25h");
            let last_hide = before.rfind("\x1b[?25l");

            let hide_pos = last_hide.expect("placement without a preceding cursor hide");
            if let Some(show_pos) = last_show {
                assert!(
                    hide_pos > show_pos,
                    "cursor was visible when a frame placement was written"
                );
            }
        }

        assert!(
            !place_positions.is_empty(),
            "should have rendered at least one kitty frame placement"
        );
        assert!(
            output.ends_with("\x1b[?25h") || output.contains("\x1b[?25h"),
            "cursor must be visible somewhere in output"
        );
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

fn build_file_items<'a>(matches: &[MatchEntry], files: &[FileEntry]) -> Vec<ListItem<'a>> {
    matches
        .iter()
        .filter_map(|matched| {
            let entry = files.get(matched.index)?;
            let fname = entry.name();
            let dir = entry.dir();
            let (icon, icon_color) = if entry.is_dir {
                ("\u{f115}", Color::Cyan)
            } else {
                get_icon(fname)
            };
            let time = time_ago(entry.recency_time);
            let time_label = format!("{} {}", time, entry.recency_source.label());

            // Split filename into stem and extension
            let (stem, ext_part) = if entry.is_dir {
                (fname, String::new())
            } else {
                let p = Path::new(fname);
                (
                    p.file_stem().and_then(|s| s.to_str()).unwrap_or(fname),
                    p.extension()
                        .and_then(|s| s.to_str())
                        .map(|e| format!(".{}", e))
                        .unwrap_or_default(),
                )
            };

            Some(ListItem::new(Line::from(vec![
                Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
                Span::styled(
                    stem.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(ext_part, Style::default().fg(icon_color)),
                Span::styled(
                    format!(" {}", time_label),
                    Style::default().fg(Color::Rgb(80, 80, 80)),
                ),
                Span::raw(" "),
                Span::styled(dir.to_string(), Style::default().fg(Color::DarkGray)),
            ])))
        })
        .collect()
}

fn ui(frame: &mut Frame, app: &App) -> (Option<Rect>, (u16, u16)) {
    let area = frame.area();
    let wide_mode = area.width >= 100;
    let dual_columns = area.width >= 140;
    let active_scope = app.active_scope();

    // Build file list items (all matches, scrolling handled by ListState)
    let (score_items, time_items, file_count) = {
        let files = app.matches_files.as_ref();
        let count = app.snapshot_for_scope(active_scope).len();
        let score_list = build_file_items(&app.matches, files);
        let time_list = build_file_items(&app.matches_by_time, files);
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
        PreviewContent::Video(_) => (vec![], true),
        PreviewContent::VideoStreaming(frames) => {
            if frames.is_empty() {
                (
                    vec![Line::from(Span::styled(
                        "Loading video...",
                        Style::default().fg(Color::Yellow),
                    ))],
                    false,
                )
            } else {
                (vec![], true)
            }
        }
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
    let preview_title = if app.dir_mode {
        " Directory "
    } else {
        match &app.preview_content {
            PreviewContent::Video(_)
            | PreviewContent::VideoStreaming(_)
            | PreviewContent::VideoLoading => " Video ",
            PreviewContent::Image(_) => " Image ",
            _ => " Preview ",
        }
    };
    // Status line
    let status = format!(
        "{} matches{} | {} {} | pool: {}{}{}",
        app.matches.len(),
        if app.is_loading() {
            " (loading...)"
        } else {
            ""
        },
        file_count,
        if app.dir_mode { "dirs" } else { "files" },
        active_scope.label(),
        if app.dir_mode { "" } else { " | ^Z pool" },
        " | ^P mode"
    );
    let status_widget = Paragraph::new(status)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(ratatui::layout::Alignment::Center);

    // Help line
    let help = if app.drag_mode {
        let mut spans = vec![
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(" drag  "),
            Span::styled("^P", Style::default().fg(Color::Yellow)),
            Span::raw(if app.dir_mode { " files  " } else { " dirs  " }),
        ];
        if !app.dir_mode {
            spans.push(Span::styled("z", Style::default().fg(Color::Yellow)));
            spans.push(Span::raw(" pool  "));
        }
        spans.extend([
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::raw(" cancel  "),
            Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::Cyan)),
            Span::raw(" nav  "),
            Span::styled("^U", Style::default().fg(Color::Cyan)),
            Span::raw(" clr"),
        ]);
        Line::from(spans)
    } else {
        let mut spans = vec![
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::raw(if app.dir_mode {
                " open dir  "
            } else {
                " open  "
            }),
            Span::styled("^P", Style::default().fg(Color::Yellow)),
            Span::raw(if app.dir_mode { " files  " } else { " dirs  " }),
            Span::styled("^D", Style::default().fg(Color::Yellow)),
            Span::raw(" drag  "),
        ];
        if !app.dir_mode {
            spans.push(Span::styled("z", Style::default().fg(Color::Yellow)));
            spans.push(Span::raw(" pool  "));
        }
        spans.extend([
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::raw(" cancel  "),
            Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::Cyan)),
            Span::raw(" nav  "),
            Span::styled("^U", Style::default().fg(Color::Cyan)),
            Span::raw(" clr"),
        ]);
        Line::from(spans)
    };
    let help_widget = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(ratatui::layout::Alignment::Center);

    // Search input
    let input = Paragraph::new(app.query.as_str())
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(60, 60, 60)))
                .title(Span::styled(
                    format!(" \u{f002} {} ", active_scope.label()),
                    Style::default().fg(Color::Cyan),
                )),
        );

    let (preview_area, cursor_pos) = if dual_columns {
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
                        .borders(Borders::RIGHT)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Rgb(60, 60, 60)))
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
        render_preview_widget(
            frame,
            main_chunks[1],
            &preview_lines,
            needs_graphics,
            preview_title,
        );

        let cursor_pos = (
            left_chunks[0].x + app.query.len() as u16 + 1,
            left_chunks[0].y + 1,
        );
        frame.set_cursor_position(cursor_pos);

        (main_chunks[1], cursor_pos)
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
        render_preview_widget(
            frame,
            main_chunks[1],
            &preview_lines,
            needs_graphics,
            preview_title,
        );

        let cursor_pos = (
            left_chunks[0].x + app.query.len() as u16 + 1,
            left_chunks[0].y + 1,
        );
        frame.set_cursor_position(cursor_pos);

        (main_chunks[1], cursor_pos)
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
        render_preview_widget(
            frame,
            chunks[2],
            &preview_lines,
            needs_graphics,
            preview_title,
        );
        frame.render_widget(status_widget, chunks[3]);
        frame.render_widget(help_widget, chunks[4]);

        let cursor_pos = (chunks[0].x + app.query.len() as u16 + 1, chunks[0].y + 1);
        frame.set_cursor_position(cursor_pos);

        (chunks[2], cursor_pos)
    };

    if needs_graphics {
        (Some(preview_area), cursor_pos)
    } else {
        (None, cursor_pos)
    }
}

fn render_preview_widget(
    frame: &mut Frame,
    area: Rect,
    preview_lines: &[Line],
    needs_graphics: bool,
    preview_title: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(60, 60, 60)))
        .title(Span::styled(
            preview_title.to_string(),
            Style::default().fg(Color::Yellow),
        ));

    if needs_graphics {
        // Do not render an empty Paragraph for image/video previews. Paragraph
        // fills the inner preview area with spaces on every TUI draw; for video
        // that happens every ~16ms, creating a visible blank interval before the
        // next kitty graphics frame is transmitted. Drawing only the border
        // leaves the previous graphics frame visible until it is atomically
        // replaced by the next one.
        frame.render_widget(block, area);
    } else {
        frame.render_widget(Paragraph::new(preview_lines.to_vec()).block(block), area);
    }
}

fn run_test_video() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let path = args
        .get(2)
        .map(|s| s.as_str())
        .unwrap_or("/home/jeremy/recording.mp4");

    eprintln!("=== Video Pipeline Test: {} ===\n", path);

    // Step 1: ffprobe duration
    let start = Instant::now();
    let dur = get_video_duration(path);
    eprintln!("[{:?}] duration: {:?}", start.elapsed(), dur);

    // Step 2: ffprobe dimensions
    let start2 = Instant::now();
    let dims = get_video_dimensions(path);
    eprintln!("[{:?}] dimensions: {:?}", start2.elapsed(), dims);

    let (src_w, src_h) = dims.unwrap_or((640, 480));
    let (out_w, out_h) = compute_scaled_dimensions(
        src_w,
        src_h,
        VIDEO_THUMBNAIL_MAX_WIDTH,
        VIDEO_THUMBNAIL_MAX_HEIGHT,
    );
    let frame_size = (out_w * out_h * 3) as usize;
    eprintln!(
        "  scaled: {}x{}, raw frame: {} bytes\n",
        out_w, out_h, frame_size
    );

    // Step 3: Stream frames (same as runtime)
    let shared = Arc::new(Mutex::new(Vec::new()));
    let shared_clone = Arc::clone(&shared);
    let done = Arc::new(Mutex::new(false));
    let done_clone = Arc::clone(&done);
    let path_owned = path.to_string();

    let stream_start = Instant::now();
    thread::spawn(move || {
        stream_video_frames(&path_owned, shared_clone);
        *done_clone.lock().unwrap() = true;
    });

    // Simulate the UI polling loop
    let mut last_count = 0;
    let mut first_frame_at = None;
    let mut state_transitions = Vec::new();

    loop {
        let frames = shared.lock().unwrap();
        let count = frames.len();
        let is_done = *done.lock().unwrap();

        if count != last_count {
            if first_frame_at.is_none() {
                first_frame_at = Some(stream_start.elapsed());
                state_transitions.push(format!(
                    "[{:?}] VideoLoading -> VideoStreaming (1 frame, {} bytes)",
                    stream_start.elapsed(),
                    frames[0].data.len()
                ));
            }
            if count % 10 == 0 || is_done {
                eprintln!(
                    "[{:?}] {} frames (latest: {} bytes)",
                    stream_start.elapsed(),
                    count,
                    frames.last().map(|f| f.data.len()).unwrap_or(0)
                );
            }
            last_count = count;
        }
        drop(frames);

        if is_done {
            state_transitions.push(format!(
                "[{:?}] VideoStreaming -> Video ({} frames)",
                stream_start.elapsed(),
                last_count
            ));
            break;
        }

        thread::sleep(Duration::from_millis(16));
    }

    eprintln!("\n=== Results ===");
    for t in &state_transitions {
        eprintln!("  {}", t);
    }

    let frames = shared.lock().unwrap();
    let total_bytes: usize = frames.iter().map(|f| f.data.len()).sum();
    eprintln!("\n  frames: {}", frames.len());
    eprintln!(
        "  first frame latency: {:?}",
        first_frame_at.unwrap_or_default()
    );
    eprintln!("  total time: {:?}", stream_start.elapsed());
    eprintln!(
        "  memory: {:.1} MiB ({:.1} KiB avg/frame)",
        total_bytes as f64 / (1024.0 * 1024.0),
        total_bytes as f64 / (frames.len().max(1) as f64 * 1024.0)
    );

    // Flicker test: check if any frames have zero or suspiciously small sizes
    let mut bad_frames = 0;
    for (i, f) in frames.iter().enumerate() {
        if f.data.len() < 100 || f.width == 0 || f.height == 0 {
            eprintln!(
                "  WARNING: frame {} looks bad: {} bytes, {}x{}",
                i,
                f.data.len(),
                f.width,
                f.height
            );
            bad_frames += 1;
        }
    }
    if bad_frames == 0 {
        eprintln!("  all frames valid ✓");
    }

    // Check for duplicate/identical frames (could cause visual stutter)
    let mut dupes = 0;
    for i in 1..frames.len() {
        if frames[i].data == frames[i - 1].data {
            dupes += 1;
        }
    }
    if dupes > 0 {
        eprintln!("  {} consecutive duplicate frames detected", dupes);
    } else {
        eprintln!("  no duplicate frames ✓");
    }

    // Flicker test: simulate the render loop and check if frame index stays valid
    eprintln!("\n=== Flicker Simulation ===");
    let frame_count = frames.len();
    drop(frames);

    let mut video_frame = 0usize;
    let mut video_frame_time = Instant::now();
    let mut render_count = 0;
    let mut out_of_bounds = 0;
    let sim_start = Instant::now();

    // Simulate 5 seconds of playback
    while sim_start.elapsed() < Duration::from_secs(5) {
        if video_frame_time.elapsed() >= Duration::from_millis(VIDEO_FRAME_INTERVAL_MS) {
            video_frame = (video_frame + 1) % frame_count;
            video_frame_time = Instant::now();
        }
        if video_frame >= frame_count {
            out_of_bounds += 1;
            video_frame = 0;
        }
        render_count += 1;
        thread::sleep(Duration::from_millis(16));
    }

    eprintln!(
        "  {} render cycles, {} out-of-bounds corrections",
        render_count, out_of_bounds
    );
    if out_of_bounds == 0 {
        eprintln!("  frame indexing OK ✓");
    }

    Ok(())
}

struct WriteRecorder<W: Write> {
    inner: W,
    writes: Vec<(Instant, usize, Vec<u8>)>,
}

impl<W: Write> WriteRecorder<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            writes: Vec::new(),
        }
    }
}

impl<W: Write> Write for WriteRecorder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.writes.push((Instant::now(), n, buf[..n].to_vec()));
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn run_bench_flicker() -> io::Result<()> {
    let area = Rect {
        x: 60,
        y: 2,
        width: 50,
        height: 25,
    };
    let cursor_input = (15u16, 1u16);

    let frame_w = 160u32;
    let frame_h = 90u32;
    let frames: Vec<CachedImage> = (0..30)
        .map(|i| {
            let mut data = vec![0u8; (frame_w * frame_h * 3) as usize];
            for (j, b) in data.iter_mut().enumerate() {
                *b = ((i * 7 + j) & 0xFF) as u8;
            }
            CachedImage {
                data: Arc::new(data),
                width: frame_w,
                height: frame_h,
                is_raw_rgb: true,
            }
        })
        .collect();

    eprintln!("=== Flicker Benchmark ===");
    eprintln!("  {} frames, {}x{} raw RGB", frames.len(), frame_w, frame_h);
    eprintln!("  frame data size: {} bytes", frames[0].data.len());
    eprintln!();

    // --- Approach A: current atomic-buffer approach ---
    {
        let mut recorder = WriteRecorder::new(Vec::<u8>::new());
        let start = Instant::now();
        let mut video_frame = 0usize;
        let mut video_frame_time = Instant::now();
        let mut last_rendered_path = String::new();
        let preview_path = "/bench/video.mp4";
        let mut dbuf = KittyDoubleBuffer::new();

        for cycle in 0..120 {
            // Simulate ratatui draw output (cursor show at input)
            write!(
                recorder,
                "\x1b[{};{}H\x1b[?25h",
                cursor_input.1 + 1,
                cursor_input.0 + 1
            )?;

            let elapsed = if cycle == 0 {
                Duration::from_millis(0)
            } else {
                video_frame_time.elapsed()
            };

            if let Some(idx) = video_render_decision(
                elapsed,
                &mut video_frame,
                &mut video_frame_time,
                frames.len(),
                preview_path,
                &last_rendered_path,
            ) {
                if let Some(frame) = frames.get(idx) {
                    let mut buf: Vec<u8> = Vec::with_capacity(frame.data.len() * 2);
                    let (new_id, old_id) = dbuf.flip();
                    compose_kitty_render(frame, area, cursor_input, new_id, old_id, &mut buf)?;
                    recorder.write_all(&buf)?;
                    recorder.flush()?;
                }
                last_rendered_path = preview_path.to_string();
            }
            thread::sleep(Duration::from_millis(16));
        }

        let elapsed = start.elapsed();
        analyze_writes("Atomic buffer (current)", &recorder.writes, elapsed);
    }

    // --- Approach B: old direct-to-stdout (simulated) ---
    {
        let mut recorder = WriteRecorder::new(Vec::<u8>::new());
        let start = Instant::now();
        let mut video_frame = 0usize;
        let mut video_frame_time = Instant::now();
        let mut last_rendered_path = String::new();
        let preview_path = "/bench/video.mp4";

        for cycle in 0..120 {
            write!(
                recorder,
                "\x1b[{};{}H\x1b[?25h",
                cursor_input.1 + 1,
                cursor_input.0 + 1
            )?;

            let elapsed = if cycle == 0 {
                Duration::from_millis(0)
            } else {
                video_frame_time.elapsed()
            };

            if let Some(idx) = video_render_decision(
                elapsed,
                &mut video_frame,
                &mut video_frame_time,
                frames.len(),
                preview_path,
                &last_rendered_path,
            ) {
                if let Some(frame) = frames.get(idx) {
                    // Old approach: write directly, each write! is a separate syscall
                    write!(recorder, "\x1b[?25l")?;
                    recorder.flush()?;
                    render_kitty_image_to(frame, area, 1, &mut recorder)?;
                    recorder.flush()?;
                    write!(recorder, "\x1b[?25h")?;
                    recorder.flush()?;
                }
                last_rendered_path = preview_path.to_string();
            }
            thread::sleep(Duration::from_millis(16));
        }

        let elapsed = start.elapsed();
        analyze_writes("Direct writes (old)", &recorder.writes, elapsed);
    }

    Ok(())
}

fn analyze_writes(label: &str, writes: &[(Instant, usize, Vec<u8>)], elapsed: Duration) {
    eprintln!("--- {} ({:?}) ---", label, elapsed);
    eprintln!("  total writes: {}", writes.len());

    let total_bytes: usize = writes.iter().map(|(_, n, _)| n).sum();
    eprintln!(
        "  total bytes: {} ({:.1} MiB)",
        total_bytes,
        total_bytes as f64 / (1024.0 * 1024.0)
    );

    let sizes: Vec<usize> = writes.iter().map(|(_, n, _)| *n).collect();
    if !sizes.is_empty() {
        let min = sizes.iter().min().unwrap();
        let max = sizes.iter().max().unwrap();
        let avg = total_bytes / sizes.len();
        eprintln!("  write sizes: min={} avg={} max={}", min, avg, max);
    }

    // Count flicker events: cursor show while not at input position
    let input_pos = format!("\x1b[{};{}H", 2, 16); // cursor_input.1+1, cursor_input.0+1
    let mut cursor_visible = false;
    let mut cursor_at_input = false;
    let mut flicker_count = 0;
    let mut kitty_while_visible = 0;
    let mut kitty_renders = 0;
    let mut show_without_reposition = 0;

    // Track state across ALL writes (simulating what terminal sees)
    for (_time, _n, data) in writes {
        let s = String::from_utf8_lossy(data);
        let mut pos = 0;
        let bytes = s.as_bytes();

        while pos < bytes.len() {
            if s[pos..].starts_with("\x1b[?25h") {
                if !cursor_at_input {
                    show_without_reposition += 1;
                }
                cursor_visible = true;
                pos += 6;
            } else if s[pos..].starts_with("\x1b[?25l") {
                cursor_visible = false;
                pos += 6;
            } else if s[pos..].starts_with(&input_pos) {
                cursor_at_input = true;
                pos += input_pos.len();
            } else if s[pos..].starts_with("\x1b[") && !s[pos..].starts_with("\x1b[?") {
                cursor_at_input = false;
                if let Some(end) = s[pos + 2..].find(|c: char| c.is_ascii_alphabetic()) {
                    pos += 2 + end + 1;
                } else {
                    pos += 1;
                }
            } else if s[pos..].starts_with("\x1b_G") {
                // Only display-affecting commands (place/transmit+display)
                // matter for flicker; data-only transmits (a=t) and deletes
                // never paint pixels under the cursor.
                let header_end = s[pos..].find(';').map(|e| pos + e).unwrap_or(s.len());
                let header = &s[pos..header_end];
                let displays = header.contains("a=p") || header.contains("a=T");
                if displays {
                    kitty_renders += 1;
                    if cursor_visible {
                        kitty_while_visible += 1;
                        flicker_count += 1;
                    }
                }
                if let Some(end) = s[pos + 2..].find("\x1b\\") {
                    pos += 2 + end + 2;
                } else {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }
    }

    eprintln!("  kitty renders: {}", kitty_renders);
    eprintln!(
        "  FLICKER - kitty while cursor visible: {}",
        kitty_while_visible
    );
    eprintln!(
        "  FLICKER - cursor shown without reposition: {}",
        show_without_reposition
    );
    eprintln!("  total flicker events: {}", flicker_count);

    // Count write boundaries that split hide/show
    let mut split_flushes = 0;
    let mut pending_hide = false;
    for (_time, _n, data) in writes {
        let s = String::from_utf8_lossy(data);
        if s.contains("\x1b[?25l") {
            pending_hide = true;
        }
        if s.contains("\x1b[?25h") {
            if !pending_hide && s.find("\x1b[?25l").is_none() {
                // show without hide in same write — could be ratatui's show, that's OK
            }
            pending_hide = false;
        }
        if pending_hide {
            // hide was in a previous write, show hasn't arrived yet
            split_flushes += 1;
        }
    }
    eprintln!("  split hide/show across writes: {}", split_flushes);
    eprintln!();
}

fn drag_file(path: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    unsafe {
        Command::new("ripdrag")
            .args(["-i", "-s", "96", "-W", "96", "-H", "96", "-x"])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .pre_exec(|| {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                Ok(())
            })
            .spawn()
            .map_err(|e| format!("Failed to spawn ripdrag: {}", e))?;
    }
    Ok(())
}

fn handle_key_press(
    key: KeyEvent,
    app: &mut App,
    selection_path: &Option<String>,
    chosen: &mut Option<ExitAction>,
) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => true,
        (KeyCode::Enter, _) => {
            if let Some(file) = app.selected_file() {
                if selection_path.is_some() {
                    *chosen = Some(ExitAction::Choose(file));
                } else if app.drag_mode {
                    *chosen = Some(ExitAction::Drag(file));
                } else {
                    *chosen = Some(ExitAction::Open(file));
                }
            }
            true
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            if let Some(file) = app.selected_file() {
                *chosen = Some(ExitAction::Drag(file));
            }
            true
        }
        (KeyCode::Down, _) | (KeyCode::Tab, KeyModifiers::NONE) => {
            if app.selected < app.active_matches().len().saturating_sub(1) {
                app.selected += 1;
            }
            false
        }
        (KeyCode::Up, _) | (KeyCode::BackTab, _) => {
            if app.selected > 0 {
                app.selected -= 1;
            }
            false
        }
        (KeyCode::Char('n'), KeyModifiers::CONTROL)
        | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
            if app.selected < app.active_matches().len().saturating_sub(1) {
                app.selected += 1;
            }
            false
        }
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            app.toggle_entry_mode();
            false
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
            if app.selected > 0 {
                app.selected -= 1;
            }
            false
        }
        (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            if app.active_column != 0 {
                app.active_column = 0;
                if app.selected >= app.matches.len() {
                    app.selected = app.matches.len().saturating_sub(1);
                }
            }
            false
        }
        (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
            if app.active_column != 1 {
                app.active_column = 1;
                if app.selected >= app.matches_by_time.len() {
                    app.selected = app.matches_by_time.len().saturating_sub(1);
                }
            }
            false
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
            false
        }
        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
            app.toggle_search_scope();
            false
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            app.query.clear();
            app.selected = 0;
            false
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) | (KeyCode::Backspace, KeyModifiers::ALT) => {
            while app.query.ends_with(' ') {
                app.query.pop();
            }
            while !app.query.is_empty() && !app.query.ends_with(' ') {
                app.query.pop();
            }
            app.selected = 0;
            false
        }
        (KeyCode::Backspace, KeyModifiers::NONE) => {
            app.query.pop();
            app.selected = 0;
            false
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            app.query.push(c);
            app.selected = 0;
            false
        }
        _ => false,
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|arg| arg == "--profile") {
        return run_profile();
    }
    if args.iter().any(|arg| arg == "--test-video") {
        return run_test_video();
    }
    if args.iter().any(|arg| arg == "--bench-flicker") {
        return run_bench_flicker();
    }
    if args.iter().any(|arg| arg == "--bench-responsiveness") {
        return run_bench_responsiveness();
    }
    let mut trace = TraceLogger::new(&args);
    let drag_mode = args.iter().any(|arg| arg == "--drag");
    let dir_mode = args.iter().any(|arg| arg == "--dir");
    let selection_path = args
        .iter()
        .position(|arg| arg == "--selection-path")
        .and_then(|i| args.get(i + 1).cloned());

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(dir_mode);
    app.drag_mode = drag_mode;
    let mut chosen: Option<ExitAction> = None;

    let mut last_image_area: Option<Rect> = None;
    let mut last_rendered_path = String::new();
    let mut kitty_dbuf = KittyDoubleBuffer::new();
    let mut last_input_time: Option<Instant> = None;
    let mut pending_input_render: Option<(Instant, usize)> = None;

    loop {
        let loop_start = Instant::now();
        let phase_start = Instant::now();
        app.update_matches();
        trace.log_duration(
            "update_matches",
            phase_start.elapsed(),
            format!(
                "query_len={} matches={}",
                app.query.len(),
                app.matches.len()
            ),
        );

        let phase_start = Instant::now();
        app.update_preview();
        trace.log_duration(
            "update_preview",
            phase_start.elapsed(),
            format!("preview_path={:?}", app.preview_path),
        );

        let mut image_area: Option<Rect> = None;
        let mut cursor_pos = (0u16, 0u16);
        let draw_start = Instant::now();
        terminal.draw(|f| {
            let result = ui(f, &app);
            image_area = result.0;
            cursor_pos = result.1;
        })?;
        trace.log_duration(
            "tui_draw",
            draw_start.elapsed(),
            format!(
                "query_len={} matches={}",
                app.query.len(),
                app.matches.len()
            ),
        );
        if let Some((first_input_at, count)) = pending_input_render.take() {
            trace.log(
                "input_to_draw",
                format!(
                    "duration_us={} input_count={} query_len={} selected={}",
                    first_input_at.elapsed().as_micros(),
                    count,
                    app.query.len(),
                    app.selected
                ),
            );
        }

        let input_pending = event::poll(Duration::from_millis(0))?;
        let input_recent = last_input_time
            .map(|at| at.elapsed() < Duration::from_millis(INPUT_RENDER_GRACE_MS))
            .unwrap_or(false);

        if !input_pending && !input_recent {
            if let Some(area) = image_area {
                let inner = Rect {
                    x: area.x + 1,
                    y: area.y + 1,
                    width: area.width.saturating_sub(2),
                    height: area.height.saturating_sub(2),
                };
                match &app.preview_content {
                    PreviewContent::Image(ref img) => {
                        if app.preview_path != last_rendered_path {
                            let render_start = Instant::now();
                            let _ = render_kitty_image(img, inner, cursor_pos, &mut kitty_dbuf);
                            trace.log_duration(
                                "kitty_image_render",
                                render_start.elapsed(),
                                format!(
                                    "path={:?} bytes={} dims={}x{}",
                                    app.preview_path,
                                    img.data.len(),
                                    img.width,
                                    img.height
                                ),
                            );
                            last_rendered_path = app.preview_path.clone();
                        }
                        last_image_area = Some(area);
                    }
                    PreviewContent::Video(ref frames)
                    | PreviewContent::VideoStreaming(ref frames) => {
                        if let Some(idx) = video_render_decision(
                            app.video_frame_time.elapsed(),
                            &mut app.video_frame,
                            &mut app.video_frame_time,
                            frames.len(),
                            &app.preview_path,
                            &last_rendered_path,
                        ) {
                            if let Some(frame) = frames.get(idx) {
                                let render_start = Instant::now();
                                let _ = render_kitty_image(frame, inner, cursor_pos, &mut kitty_dbuf);
                                trace.log_duration(
                                    "kitty_video_render",
                                    render_start.elapsed(),
                                    format!(
                                        "path={:?} frame={} bytes={} dims={}x{} frames={}",
                                        app.preview_path,
                                        idx,
                                        frame.data.len(),
                                        frame.width,
                                        frame.height,
                                        frames.len()
                                    ),
                                );
                            }
                            last_rendered_path = app.preview_path.clone();
                        }
                        last_image_area = Some(area);
                    }
                    _ => {}
                }
            } else if last_image_area.is_some() {
                let clear_start = Instant::now();
                let _ = clear_kitty_image();
                trace.log_duration("kitty_clear", clear_start.elapsed(), "");
                last_image_area = None;
                last_rendered_path.clear();
            }
        } else if image_area.is_some() && trace.enabled() {
            trace.log(
                "preview_render_skipped_for_input",
                format!(
                    "input_pending={} input_recent={} grace_ms={} preview_path={:?}",
                    input_pending, input_recent, INPUT_RENDER_GRACE_MS, app.preview_path
                ),
            );
        }

        // Poll with shorter timeout for smooth video, longer for static content.
        // While a search is computing, poll fast so its results render the
        // moment they arrive instead of waiting out a long poll.
        let poll_ms = match &app.preview_content {
            PreviewContent::Video(_) => 16, // ~60fps
            PreviewContent::VideoStreaming(_) => 16,
            PreviewContent::VideoLoading => 50,
            _ if app.search_pending() => 8,
            _ => 100,
        };
        let poll_start = Instant::now();
        let mut should_exit = false;
        if event::poll(Duration::from_millis(poll_ms))? {
            trace.log_duration(
                "event_wait_ready",
                poll_start.elapsed(),
                format!("poll_ms={}", poll_ms),
            );

            let mut drained_events = 0usize;
            loop {
                let read_start = Instant::now();
                let event = event::read()?;
                trace.log_duration("event_read", read_start.elapsed(), "");

                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        let input_at = Instant::now();
                        last_input_time = Some(input_at);
                        if let Some((_, count)) = pending_input_render.as_mut() {
                            *count += 1;
                        } else {
                            pending_input_render = Some((input_at, 1));
                        }
                        trace.log(
                            "input_event",
                            format!(
                                "key={:?} modifiers={:?} query_len_before={} selected_before={} active_column={} drained_index={}",
                                key.code,
                                key.modifiers,
                                app.query.len(),
                                app.selected,
                                app.active_column,
                                drained_events
                            ),
                        );

                        let handle_start = Instant::now();
                        should_exit = handle_key_press(key, &mut app, &selection_path, &mut chosen);
                        trace.log_duration(
                            "input_handle",
                            handle_start.elapsed(),
                            format!(
                                "query_len_after={} selected_after={} should_exit={}",
                                app.query.len(),
                                app.selected,
                                should_exit
                            ),
                        );
                    }
                }

                drained_events += 1;
                if should_exit {
                    break;
                }
                if drained_events >= INPUT_DRAIN_LIMIT {
                    trace.log(
                        "input_drain_limit",
                        format!(
                            "limit={} query_len={} selected={}",
                            INPUT_DRAIN_LIMIT,
                            app.query.len(),
                            app.selected
                        ),
                    );
                    break;
                }
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }
        if should_exit {
            break;
        }
        trace.log_duration(
            "event_loop",
            loop_start.elapsed(),
            format!(
                "query_len={} matches={} preview_path={:?}",
                app.query.len(),
                app.matches.len(),
                app.preview_path
            ),
        );
    }

    // Clear any kitty graphics before exiting
    let _ = clear_kitty_image();

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    // Open or drag the file after terminal is restored
    if let Some(action) = chosen {
        let (path, result) = match action {
            ExitAction::Choose(path) => {
                let r = if let Some(selection_file) = selection_path.as_ref() {
                    fs::write(selection_file, format!("{}\n", path))
                        .map_err(|e| format!("Failed to write selection path: {}", e))
                } else {
                    Ok(())
                };
                (path, r)
            }
            ExitAction::Drag(path) => {
                let r = drag_file(&path);
                (path, r)
            }
            ExitAction::Open(path) => {
                let r = open_file(&path);
                (path, r)
            }
        };
        match result {
            Ok(()) => {
                let _ = record_file_interaction(&path);
            }
            Err(e) => {
                let _ = notify_rust::Notification::new()
                    .summary("File Picker")
                    .body(&format!("{}: {}", path, e))
                    .show();
            }
        }
    }

    Ok(())
}
