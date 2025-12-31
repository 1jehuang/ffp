use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use rapidfuzz::fuzz::ratio;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use std::{
    cmp::Ordering,
    env,
    fs,
    io::{self, BufRead, BufReader},
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

struct App {
    query: String,
    files: Arc<Mutex<Vec<FileEntry>>>,
    matches: Vec<MatchEntry>,
    selected: usize,
    loading: Arc<Mutex<bool>>,
    last_query: String,
    last_file_count: usize,
    preview_path: String,
    preview_lines: Vec<String>,
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
            preview_lines: Vec::new(),
        }
    }

    fn update_preview(&mut self) {
        let current_path = self.selected_file().unwrap_or_default();
        if current_path == self.preview_path {
            return; // Already cached
        }
        self.preview_path = current_path.clone();
        self.preview_lines = read_preview(&current_path, 10);
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
    const MAX_BYTES: usize = 4096; // Don't read too much

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

fn ui(frame: &mut Frame, app: &App) {
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

    // Build preview widget
    let preview_lines: Vec<Line> = app
        .preview_lines
        .iter()
        .map(|s| Line::from(Span::styled(s.as_str(), Style::default().fg(Color::Gray))))
        .collect();
    let preview = Paragraph::new(preview_lines)
        .block(Block::default().borders(Borders::ALL).title(Span::styled(
            " Preview ",
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

    if wide_mode {
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
    } else {
        // Narrow: preview at bottom
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(8),
                Constraint::Length(1),
                Constraint::Length(1),
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

    loop {
        app.update_matches();
        app.update_preview();
        terminal.draw(|f| ui(f, &app))?;

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
