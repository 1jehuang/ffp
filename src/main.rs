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

fn get_icon(filename: &str) -> &'static str {
    let ext = Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "pdf" => "",
        "doc" | "docx" => "󰈬",
        "md" => "",
        "txt" => "",
        "cpp" | "cc" | "cxx" => "",
        "c" | "h" => "",
        "py" => "",
        "js" => "",
        "ts" => "",
        "rs" => "",
        "go" => "",
        "java" => "",
        "sh" | "bash" | "zsh" => "",
        "html" => "",
        "css" => "",
        "json" => "",
        "yaml" | "yml" => "",
        "toml" => "",
        "png" | "jpg" | "jpeg" | "gif" | "svg" => "",
        "mp3" | "wav" | "flac" => "",
        "mp4" | "mkv" | "avi" => "",
        "zip" | "tar" | "gz" | "xz" => "",
        _ if filename.starts_with('.') => "",
        _ => "",
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Search input
    let input = Paragraph::new(app.query.as_str())
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" ", Style::default().fg(Color::Cyan))),
        );
    frame.render_widget(input, chunks[0]);

    // File list
    let files = app.files.lock().unwrap();
    let file_count = files.len();
    let items: Vec<ListItem> = app
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
                Span::styled(fname, style.add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(dir, Style::default().fg(Color::DarkGray)),
            ]))
            .style(style))
        })
        .collect();

    let list = List::new(items).block(Block::default());
    frame.render_widget(list, chunks[1]);

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
    frame.render_widget(status_widget, chunks[2]);

    // Keyboard help line
    let help = Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" open  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" cancel  "),
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::raw(" navigate  "),
        Span::styled("^U", Style::default().fg(Color::Cyan)),
        Span::raw(" clear  "),
        Span::styled("^W", Style::default().fg(Color::Cyan)),
        Span::raw(" del word"),
    ]);
    let help_widget = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(help_widget, chunks[3]);

    // Set cursor position
    frame.set_cursor_position((
        chunks[0].x + app.query.len() as u16 + 1,
        chunks[0].y + 1,
    ));
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
                    (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
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
                    (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                        // Delete word
                        while app.query.ends_with(' ') {
                            app.query.pop();
                        }
                        while !app.query.is_empty() && !app.query.ends_with(' ') {
                            app.query.pop();
                        }
                        app.selected = 0;
                    }
                    (KeyCode::Backspace, _) => {
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
