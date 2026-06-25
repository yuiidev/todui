use std::{
    env, fmt, fs,
    io::{self, Write, stdout},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use notify::{Config, Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use syntect::{
    easy::HighlightLines,
    highlighting::{
        Color as SyntectColor, FontStyle, Highlighter, Style as SyntectStyle, Theme, ThemeSet,
    },
    parsing::{Scope, SyntaxSet},
};
use uuid::Uuid;

const DATA_DIR: &str = "data";
const SETTINGS_FILE: &str = "settings.json";
const TODO_INSTRUCTIONS_FILE: &str = "INSTRUCTIONS.md";
const LLM_API_KEY_ENV: &str = "TODUI_LLM_API_KEY";
const CHAT_BAR_OUTPUT_SCHEMA_FILE: &str = "schemas/chat-bar-todo.schema.json";
const CHAT_BAR_ERROR_LOG_FILE: &str = "chat-bar-error.log";
const HIDE_COMPLETED_AFTER_DAYS: i64 = 14;
const DEFAULT_TASK_CONTENT_WRAP_COLS: usize = 120;
const MIN_TASK_CONTENT_WRAP_COLS: usize = 20;
const TASK_CONTENT_WRAP_COL_STEP: usize = 10;
const DEFAULT_LLM_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_LLM_MODEL: &str = "gpt-5.5";
const LLM_REQUEST_TIMEOUT_SECS: u64 = 60;
const CHAT_BAR_JSON_HARNESS: &str = r#"You are the todui chat bar JSON harness.
Convert the user's call, meeting, or conversation notes into one new todo draft.

Output contract:
- Return only one valid JSON object.
- Do not include Markdown, code fences, comments, explanations, or surrounding text.
- The first non-whitespace character must be `{` and the last non-whitespace character must be `}`.
- Required keys: "title", "content", "labels", "branch", "due_at".
- Do not include "id", "created_at", "updated_at", or "completed_at".
- Use null for "branch" or "due_at" when absent."#;

fn main() -> Result<()> {
    let mut terminal = init_terminal()?;
    let app_result = App::new(PathBuf::from(DATA_DIR), PathBuf::from(SETTINGS_FILE))
        .and_then(|app| run_app(&mut terminal, app));
    let restore_result = restore_terminal(&mut terminal);

    if let Err(error) = restore_result {
        eprintln!("failed to restore terminal: {error:#}");
    }

    app_result
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn init_terminal() -> Result<Tui> {
    enable_raw_mode().context("enable raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("enter alternate screen")?;
    Terminal::new(CrosstermBackend::new(out)).context("create terminal")
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")
}

fn run_app(terminal: &mut Tui, mut app: App) -> Result<()> {
    while !app.should_quit {
        app.sync_project_watcher();
        app.reload_changed_project();
        app.poll_llm_response();
        app.expire_status_message();

        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .context("draw ui")?;

        if event::poll(Duration::from_millis(200)).context("poll terminal event")? {
            match event::read().context("read terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Project {
    title: String,
    description: String,
    labels: Vec<String>,
    tasks: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Task {
    id: String,
    title: String,
    content: String,
    labels: Vec<String>,
    branch: Option<String>,
    created_at: String,
    updated_at: Option<String>,
    completed_at: Option<String>,
    due_at: Option<String>,
}

#[derive(Debug, Clone)]
struct ProjectFile {
    file_stem: String,
    path: PathBuf,
    project: Project,
}

fn load_project_file(path: &Path) -> Result<ProjectFile> {
    let file_stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("invalid project filename {}", path.display()))?
        .to_string();
    let json = fs::read_to_string(path)
        .with_context(|| format!("read project file {}", path.display()))?;
    let project = serde_json::from_str::<Project>(&json)
        .with_context(|| format!("parse project file {}", path.display()))?;

    Ok(ProjectFile {
        file_stem,
        path: path.to_path_buf(),
        project,
    })
}

fn load_projects(data_dir: &Path) -> Result<Vec<ProjectFile>> {
    let entries = fs::read_dir(data_dir)
        .with_context(|| format!("read data directory {}", data_dir.display()))?;
    let mut projects = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in {}", data_dir.display()))?;
        let path = entry.path();

        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }

        projects.push(load_project_file(&path)?);
    }

    projects.sort_by(|left, right| {
        left.project
            .title
            .to_lowercase()
            .cmp(&right.project.title.to_lowercase())
            .then_with(|| left.file_stem.cmp(&right.file_stem))
    });

    Ok(projects)
}

fn save_project(project_file: &ProjectFile) -> Result<()> {
    let json = serde_json::to_string_pretty(&project_file.project)
        .with_context(|| format!("serialize project {}", project_file.project.title))?;
    fs::write(&project_file.path, format!("{json}\n"))
        .with_context(|| format!("write project file {}", project_file.path.display()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Settings {
    statusline_message_timeout_ms: u64,
    syntax_theme: String,
    syntax_theme_folder: String,
    use_theme_app_background: bool,
    use_theme_modal_background: bool,
    task_content_wrap_cols: usize,
    llm_backend: LlmBackend,
    codex_reasoning_effort: Option<CodexReasoningEffort>,
    codex_fast_mode: bool,
    llm_base_url: String,
    llm_model: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LlmBackend {
    CodexExec,
    Api,
}

impl Default for LlmBackend {
    fn default() -> Self {
        Self::CodexExec
    }
}

impl LlmBackend {
    fn label(self) -> &'static str {
        match self {
            Self::CodexExec => "codex_exec",
            Self::Api => "api",
        }
    }

    fn cycle(self, _direction: isize) -> Self {
        match self {
            Self::CodexExec => Self::Api,
            Self::Api => Self::CodexExec,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CodexReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl CodexReasoningEffort {
    fn label(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

fn cycle_codex_reasoning_effort(
    effort: Option<CodexReasoningEffort>,
    direction: isize,
) -> Option<CodexReasoningEffort> {
    let options = [
        None,
        Some(CodexReasoningEffort::Minimal),
        Some(CodexReasoningEffort::Low),
        Some(CodexReasoningEffort::Medium),
        Some(CodexReasoningEffort::High),
        Some(CodexReasoningEffort::Xhigh),
    ];
    let current = options
        .iter()
        .position(|option| *option == effort)
        .unwrap_or(0);
    let next = (current as isize + direction).rem_euclid(options.len() as isize) as usize;

    options[next]
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            statusline_message_timeout_ms: 3000,
            syntax_theme: "base16-ocean.dark".to_string(),
            syntax_theme_folder: "themes".to_string(),
            use_theme_app_background: true,
            use_theme_modal_background: true,
            task_content_wrap_cols: DEFAULT_TASK_CONTENT_WRAP_COLS,
            llm_backend: LlmBackend::default(),
            codex_reasoning_effort: None,
            codex_fast_mode: false,
            llm_base_url: DEFAULT_LLM_BASE_URL.to_string(),
            llm_model: DEFAULT_LLM_MODEL.to_string(),
        }
    }
}

impl Settings {
    fn statusline_message_timeout(&self) -> Duration {
        Duration::from_millis(self.statusline_message_timeout_ms)
    }
}

fn load_settings(path: &Path) -> Result<Settings> {
    if !path.exists() {
        return Ok(Settings::default());
    }

    let json = fs::read_to_string(path)
        .with_context(|| format!("read settings file {}", path.display()))?;
    serde_json::from_str(&json).with_context(|| format!("parse settings file {}", path.display()))
}

fn save_settings(path: &Path, settings: &Settings) -> Result<()> {
    let json = serde_json::to_string_pretty(settings).context("serialize settings")?;
    fs::write(path, format!("{json}\n"))
        .with_context(|| format!("write settings file {}", path.display()))
}

fn short_error_message(error: &anyhow::Error) -> &'static str {
    let text = error.to_string();

    if text.contains("title cannot be empty") {
        "empty title"
    } else if text.contains("completed_at") {
        "invalid completed_at"
    } else if text.contains("due_at") {
        "invalid due_at"
    } else if text.contains("no task selected") {
        "no task selected"
    } else if text.contains("no project selected") {
        "no project selected"
    } else if text.contains("empty prompt") {
        "empty prompt"
    } else if text.contains(LLM_API_KEY_ENV) {
        "missing api key"
    } else if text.contains("codex") {
        "codex failed; see log"
    } else if text.contains("llm") {
        "llm failed"
    } else if text.contains("draft") || text.contains("todo json") {
        "invalid llm json"
    } else if text.contains("watch") {
        "watch failed"
    } else if text.contains("parse") || text.contains("read") {
        "reload failed"
    } else if text.contains("write") || text.contains("serialize") {
        "save failed"
    } else if text.contains("delete") {
        "delete failed"
    } else {
        "action failed"
    }
}

#[derive(Debug)]
struct SyntaxResources {
    syntax_set: SyntaxSet,
    theme: Theme,
    ui_theme: UiTheme,
}

impl SyntaxResources {
    fn new(settings: &Settings) -> Result<Self> {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = load_theme_set(settings)?;
        let theme = theme_set
            .themes
            .get(&settings.syntax_theme)
            .or_else(|| theme_set.themes.get("base16-ocean.dark"))
            .or_else(|| theme_set.themes.values().next())
            .cloned()
            .ok_or_else(|| anyhow!("no syntax themes available"))?;
        let ui_theme = UiTheme::from_syntect(&theme);

        Ok(Self {
            syntax_set,
            theme,
            ui_theme,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UiTheme {
    foreground: Color,
    background: Color,
    muted: Color,
    accent: Color,
    selection: Color,
    selection_text: Color,
    error: Color,
    inline_code: Color,
}

impl UiTheme {
    fn from_syntect(theme: &Theme) -> Self {
        let settings = &theme.settings;
        let foreground = settings
            .foreground
            .or_else(|| scope_foreground(theme, &["foreground"]))
            .unwrap_or_else(|| syntect_rgb(255, 255, 255));
        let background = settings.background.unwrap_or_else(|| syntect_rgb(0, 0, 0));
        let muted = settings
            .gutter_foreground
            .or_else(|| scope_foreground(theme, &["comment"]))
            .unwrap_or_else(|| syntect_rgb(128, 128, 128));
        let accent = settings
            .accent
            .or_else(|| {
                scope_foreground(
                    theme,
                    &[
                        "support.class",
                        "entity.name.class",
                        "entity.name.type.class",
                        "entity.name.type",
                        "entity.name.function",
                        "keyword",
                        "string.other.link",
                    ],
                )
            })
            .unwrap_or(foreground);
        let selection = settings
            .selection
            .or(settings.line_highlight)
            .unwrap_or(accent);
        let inline_code = scope_foreground(
            theme,
            &[
                "markup.raw.inline",
                "markup.raw",
                "string",
                "constant.other.symbol",
            ],
        )
        .unwrap_or(accent);
        let error = settings
            .misspelling
            .or_else(|| scope_foreground(theme, &["markup.deleted", "invalid.illegal", "invalid"]))
            .unwrap_or(accent);

        Self {
            foreground: ratatui_color(foreground),
            background: ratatui_color(background),
            muted: ratatui_color(muted),
            accent: ratatui_color(accent),
            selection: ratatui_color(selection),
            selection_text: ratatui_color(settings.selection_foreground.unwrap_or(foreground)),
            error: ratatui_color(error),
            inline_code: ratatui_color(inline_code),
        }
    }

    fn style(self, use_background: bool) -> Style {
        let style = Style::default().fg(self.foreground);
        if use_background {
            style.bg(self.background)
        } else {
            style
        }
    }
}

fn scope_foreground(theme: &Theme, scopes: &[&str]) -> Option<SyntectColor> {
    let highlighter = Highlighter::new(theme);

    scopes.iter().find_map(|scope| {
        let scope = Scope::new(scope).ok()?;
        highlighter.style_mod_for_stack(&[scope]).foreground
    })
}

fn syntect_rgb(r: u8, g: u8, b: u8) -> SyntectColor {
    SyntectColor { r, g, b, a: 255 }
}

fn ratatui_color(color: SyntectColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn load_theme_set(settings: &Settings) -> Result<ThemeSet> {
    let mut theme_set = ThemeSet::load_defaults();
    let theme_folder = Path::new(&settings.syntax_theme_folder);

    if theme_folder.exists() {
        let custom_theme_set = ThemeSet::load_from_folder(theme_folder)
            .with_context(|| format!("load syntax themes from {}", theme_folder.display()))?;
        theme_set.themes.extend(custom_theme_set.themes);
    }

    Ok(theme_set)
}

fn load_theme_options(settings: &Settings) -> Result<Vec<String>> {
    let theme_folder = Path::new(&settings.syntax_theme_folder);
    if !theme_folder.exists() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(theme_folder)
        .with_context(|| format!("read syntax theme directory {}", theme_folder.display()))?;
    let mut options = Vec::new();

    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("tmTheme")
            && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        {
            options.push(stem.to_string());
        }
    }

    options.sort_by_key(|name| name.to_lowercase());
    Ok(options)
}

struct ProjectWatcher {
    watched_path: PathBuf,
    watched_file_name: String,
    receiver: Receiver<notify::Result<NotifyEvent>>,
    _watcher: RecommendedWatcher,
}

impl ProjectWatcher {
    fn new(path: &Path) -> Result<Self> {
        let watched_file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("invalid project filename {}", path.display()))?
            .to_string();
        let watched_parent = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                let _ = sender.send(event);
            },
            Config::default(),
        )
        .with_context(|| format!("create watcher for {}", path.display()))?;

        // Watch the containing directory and filter to this file so atomic file
        // replacement still produces a reload on platforms that drop inode watches.
        watcher
            .watch(&watched_parent, RecursiveMode::NonRecursive)
            .with_context(|| format!("watch project file {}", path.display()))?;

        Ok(Self {
            watched_path: path.to_path_buf(),
            watched_file_name,
            receiver,
            _watcher: watcher,
        })
    }

    fn watches(&self, path: &Path) -> bool {
        self.watched_path == path
    }

    fn has_changed(&self) -> Result<bool> {
        let mut changed = false;

        while let Ok(event) = self.receiver.try_recv() {
            let event = event
                .with_context(|| format!("watch project file {}", self.watched_path.display()))?;
            if self.is_relevant_change(&event) {
                changed = true;
            }
        }

        Ok(changed)
    }

    fn is_relevant_change(&self, event: &NotifyEvent) -> bool {
        matches!(
            event.kind,
            EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) && (event.paths.is_empty()
            || event.paths.iter().any(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == self.watched_file_name)
            }))
    }
}

impl fmt::Debug for ProjectWatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectWatcher")
            .field("watched_path", &self.watched_path)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Projects,
    ProjectDetail,
    TaskDetail,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    SyntaxTheme,
    AppBackground,
    ModalBackground,
    TaskContentWrapCols,
    LlmBackend,
    CodexReasoningEffort,
    CodexFastMode,
    LlmBaseUrl,
    LlmModel,
}

const SETTINGS_FIELDS: [SettingsField; 9] = [
    SettingsField::SyntaxTheme,
    SettingsField::AppBackground,
    SettingsField::ModalBackground,
    SettingsField::TaskContentWrapCols,
    SettingsField::LlmBackend,
    SettingsField::CodexReasoningEffort,
    SettingsField::CodexFastMode,
    SettingsField::LlmBaseUrl,
    SettingsField::LlmModel,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteTarget {
    Project,
    Task,
}

#[derive(Debug)]
enum Mode {
    Normal,
    Editing(EditForm),
    Chat(ChatForm),
    EditingSetting(SettingTextForm),
    ConfirmDelete(DeleteTarget),
}

#[derive(Debug)]
struct StatusMessage {
    text: String,
    created_at: Instant,
}

#[derive(Debug)]
struct App {
    data_dir: PathBuf,
    projects: Vec<ProjectFile>,
    settings: Settings,
    settings_path: PathBuf,
    theme_options: Vec<String>,
    syntax_resources: SyntaxResources,
    project_watcher: Option<ProjectWatcher>,
    project_index: usize,
    task_index: usize,
    settings_index: usize,
    settings_return_screen: Screen,
    show_hidden_tasks: bool,
    detail_scroll: u16,
    screen: Screen,
    mode: Mode,
    llm_receiver: Option<Receiver<LlmWorkerResult>>,
    status_message: Option<StatusMessage>,
    should_quit: bool,
}

impl App {
    fn new(data_dir: PathBuf, settings_path: PathBuf) -> Result<Self> {
        let projects = load_projects(&data_dir)?;
        let settings = load_settings(&settings_path)?;
        let theme_options = load_theme_options(&settings)?;
        let syntax_resources = SyntaxResources::new(&settings)?;
        Ok(Self {
            data_dir,
            projects,
            settings,
            settings_path,
            theme_options,
            syntax_resources,
            project_watcher: None,
            project_index: 0,
            task_index: 0,
            settings_index: 0,
            settings_return_screen: Screen::Projects,
            show_hidden_tasks: false,
            detail_scroll: 0,
            screen: Screen::Projects,
            mode: Mode::Normal,
            llm_receiver: None,
            status_message: None,
            should_quit: false,
        })
    }

    fn set_status_message(&mut self, message: impl Into<String>) {
        self.status_message = Some(StatusMessage {
            text: message.into(),
            created_at: Instant::now(),
        });
    }

    fn expire_status_message(&mut self) {
        let timeout = self.settings.statusline_message_timeout();
        if self
            .status_message
            .as_ref()
            .is_some_and(|message| message.created_at.elapsed() >= timeout)
        {
            self.status_message = None;
        }
    }

    fn status_message_text(&self) -> Option<&str> {
        self.status_message
            .as_ref()
            .map(|message| message.text.as_str())
    }

    fn sync_project_watcher(&mut self) {
        let Some(path) = self.watched_project_path() else {
            self.project_watcher = None;
            return;
        };

        if self
            .project_watcher
            .as_ref()
            .is_some_and(|watcher| watcher.watches(&path))
        {
            return;
        }

        match ProjectWatcher::new(&path) {
            Ok(watcher) => self.project_watcher = Some(watcher),
            Err(error) => {
                self.project_watcher = None;
                self.set_status_message(short_error_message(&error));
            }
        }
    }

    fn watched_project_path(&self) -> Option<PathBuf> {
        match self.screen {
            Screen::Projects | Screen::Settings => None,
            Screen::ProjectDetail | Screen::TaskDetail => {
                self.current_project().map(|project| project.path.clone())
            }
        }
    }

    fn reload_changed_project(&mut self) {
        let changed = match &self.project_watcher {
            Some(watcher) => watcher.has_changed(),
            None => return,
        };

        match changed {
            Ok(true) => {
                if let Err(error) = self.reload_current_project_from_disk() {
                    self.set_status_message(short_error_message(&error));
                }
            }
            Ok(false) => {}
            Err(error) => self.set_status_message(short_error_message(&error)),
        }
    }

    fn reload_current_project_from_disk(&mut self) -> Result<()> {
        let project_index = self.project_index;
        let project_path = self
            .projects
            .get(project_index)
            .ok_or_else(|| anyhow!("no project selected"))?
            .path
            .clone();
        let selected_task_id = self.current_task().map(|task| task.id.clone());
        let reloaded_project = load_project_file(&project_path)?;

        self.projects[project_index] = reloaded_project;
        self.restore_task_selection(selected_task_id.as_deref());
        self.set_status_message("project reloaded");
        Ok(())
    }

    fn restore_task_selection(&mut self, selected_task_id: Option<&str>) {
        let Some(project) = self.current_project() else {
            self.task_index = 0;
            self.detail_scroll = 0;
            return;
        };

        if project.project.tasks.is_empty() {
            self.task_index = 0;
            self.detail_scroll = 0;
            if self.screen == Screen::TaskDetail {
                self.screen = Screen::ProjectDetail;
            }
            return;
        }

        if let Some(id) = selected_task_id
            && let Some(index) = project.project.tasks.iter().position(|task| task.id == id)
        {
            self.task_index = index;
            self.normalize_task_selection();
            return;
        }

        self.task_index = self.task_index.min(project.project.tasks.len() - 1);
        self.normalize_task_selection();
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let result = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => {
                self.mode = Mode::Normal;
                self.handle_normal_key(key)
            }
            Mode::Editing(mut form) => match form.handle_key(key) {
                EditAction::None => {
                    self.mode = Mode::Editing(form);
                    Ok(())
                }
                EditAction::Cancel => {
                    self.mode = Mode::Normal;
                    Ok(())
                }
                EditAction::Save => {
                    self.mode = Mode::Editing(form);
                    self.save_edit_form()
                }
            },
            Mode::Chat(mut form) => match form.handle_key(key) {
                ChatAction::None => {
                    self.mode = Mode::Chat(form);
                    Ok(())
                }
                ChatAction::Cancel => {
                    self.llm_receiver = None;
                    self.mode = Mode::Normal;
                    Ok(())
                }
                ChatAction::Submit(prompt) => {
                    self.mode = Mode::Chat(form);
                    self.start_llm_todo_request(prompt)
                }
            },
            Mode::EditingSetting(mut form) => match form.handle_key(key) {
                SettingTextAction::None => {
                    self.mode = Mode::EditingSetting(form);
                    Ok(())
                }
                SettingTextAction::Cancel => {
                    self.mode = Mode::Normal;
                    Ok(())
                }
                SettingTextAction::Save => {
                    self.mode = Mode::EditingSetting(form);
                    self.save_setting_text_form()
                }
            },
            Mode::ConfirmDelete(target) => {
                self.mode = Mode::ConfirmDelete(target);
                self.handle_confirm_key(key, target)
            }
        };

        if let Err(error) = result {
            self.set_status_message(short_error_message(&error));
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.screen == Screen::Settings {
            return self.handle_settings_key(key);
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.back_or_quit(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('c') => self.open_chat_bar(),
            KeyCode::Char(' ') => self.toggle_selected_task_completion(),
            KeyCode::Char('h') => self.toggle_hidden_tasks(),
            KeyCode::Char('e') => self.start_editing_selected_task(),
            KeyCode::Char('d') => self.start_delete_confirmation(),
            KeyCode::Home => {
                self.task_index = 0;
                self.detail_scroll = 0;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.back_or_quit(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.settings_index = (self.settings_index + 1).min(SETTINGS_FIELDS.len() - 1);
                Ok(())
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.settings_index = self.settings_index.saturating_sub(1);
                Ok(())
            }
            KeyCode::Char('h') | KeyCode::Left => self.change_setting(-1),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
                self.change_setting(1)
            }
            _ => Ok(()),
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent, target: DeleteTarget) -> Result<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => match target {
                DeleteTarget::Project => self.delete_selected_project(),
                DeleteTarget::Task => self.delete_selected_task(),
            },
            KeyCode::Char('n') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn back_or_quit(&mut self) -> Result<()> {
        match self.screen {
            Screen::Projects => self.should_quit = true,
            Screen::ProjectDetail => self.screen = Screen::Projects,
            Screen::TaskDetail => self.screen = Screen::ProjectDetail,
            Screen::Settings => self.screen = self.settings_return_screen,
        }
        Ok(())
    }

    fn open_settings(&mut self) -> Result<()> {
        self.theme_options = load_theme_options(&self.settings)?;
        self.settings_return_screen = self.screen;
        self.screen = Screen::Settings;
        self.settings_index = self.settings_index.min(SETTINGS_FIELDS.len() - 1);
        Ok(())
    }

    fn open_chat_bar(&mut self) -> Result<()> {
        if self.current_project().is_none() {
            bail!("no project selected");
        }

        self.mode = Mode::Chat(ChatForm::new());
        Ok(())
    }

    fn start_editing_setting(&mut self, field: SettingsField) {
        let value = match field {
            SettingsField::LlmBaseUrl => self.settings.llm_base_url.clone(),
            SettingsField::LlmModel => self.settings.llm_model.clone(),
            SettingsField::SyntaxTheme
            | SettingsField::AppBackground
            | SettingsField::ModalBackground
            | SettingsField::TaskContentWrapCols
            | SettingsField::LlmBackend
            | SettingsField::CodexReasoningEffort
            | SettingsField::CodexFastMode => return,
        };

        self.mode = Mode::EditingSetting(SettingTextForm::new(field, value));
    }

    fn save_setting_text_form(&mut self) -> Result<()> {
        let (field, value) = match &self.mode {
            Mode::EditingSetting(form) => (form.field, form.value()?),
            _ => return Ok(()),
        };

        match field {
            SettingsField::LlmBaseUrl => {
                validate_llm_base_url(&value)?;
                self.settings.llm_base_url = value;
            }
            SettingsField::LlmModel => self.settings.llm_model = value,
            SettingsField::SyntaxTheme
            | SettingsField::AppBackground
            | SettingsField::ModalBackground
            | SettingsField::TaskContentWrapCols
            | SettingsField::LlmBackend
            | SettingsField::CodexReasoningEffort
            | SettingsField::CodexFastMode => return Ok(()),
        }

        save_settings(&self.settings_path, &self.settings)?;
        self.mode = Mode::Normal;
        self.set_status_message("settings saved");
        Ok(())
    }

    fn change_setting(&mut self, direction: isize) -> Result<()> {
        match SETTINGS_FIELDS[self.settings_index] {
            SettingsField::SyntaxTheme => self.cycle_syntax_theme(direction)?,
            SettingsField::AppBackground => {
                self.settings.use_theme_app_background = !self.settings.use_theme_app_background
            }
            SettingsField::ModalBackground => {
                self.settings.use_theme_modal_background = !self.settings.use_theme_modal_background
            }
            SettingsField::TaskContentWrapCols => self.change_task_content_wrap_cols(direction),
            SettingsField::LlmBackend => {
                self.settings.llm_backend = self.settings.llm_backend.cycle(direction)
            }
            SettingsField::CodexReasoningEffort => {
                self.settings.codex_reasoning_effort =
                    cycle_codex_reasoning_effort(self.settings.codex_reasoning_effort, direction);
            }
            SettingsField::CodexFastMode => {
                self.settings.codex_fast_mode = !self.settings.codex_fast_mode
            }
            SettingsField::LlmBaseUrl | SettingsField::LlmModel => {
                self.start_editing_setting(SETTINGS_FIELDS[self.settings_index]);
                return Ok(());
            }
        }

        save_settings(&self.settings_path, &self.settings)?;
        self.set_status_message("settings saved");
        Ok(())
    }

    fn change_task_content_wrap_cols(&mut self, direction: isize) {
        self.settings.task_content_wrap_cols = if direction < 0 {
            self.settings
                .task_content_wrap_cols
                .saturating_sub(TASK_CONTENT_WRAP_COL_STEP)
                .max(MIN_TASK_CONTENT_WRAP_COLS)
        } else {
            self.settings
                .task_content_wrap_cols
                .saturating_add(TASK_CONTENT_WRAP_COL_STEP)
        };
    }

    fn cycle_syntax_theme(&mut self, direction: isize) -> Result<()> {
        if self.theme_options.is_empty() {
            return Ok(());
        }

        let len = self.theme_options.len();
        let current = self
            .theme_options
            .iter()
            .position(|theme| theme == &self.settings.syntax_theme)
            .unwrap_or(0);
        let next = (current as isize + direction).rem_euclid(len as isize) as usize;
        let previous = self.settings.syntax_theme.clone();

        self.settings.syntax_theme = self.theme_options[next].clone();
        match SyntaxResources::new(&self.settings) {
            Ok(resources) => self.syntax_resources = resources,
            Err(error) => {
                self.settings.syntax_theme = previous;
                return Err(error);
            }
        }

        Ok(())
    }

    fn start_llm_todo_request(&mut self, prompt: String) -> Result<()> {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            bail!("empty prompt");
        }

        if self.llm_receiver.is_some() {
            return Ok(());
        }

        let project = self
            .current_project()
            .ok_or_else(|| anyhow!("no project selected"))?
            .project
            .clone();
        let instructions_path = self.data_dir.join(TODO_INSTRUCTIONS_FILE);
        let instructions = fs::read_to_string(&instructions_path)
            .with_context(|| format!("read {}", instructions_path.display()))?;
        let api_key = if self.settings.llm_backend == LlmBackend::Api {
            let api_key = env::var(LLM_API_KEY_ENV)
                .with_context(|| format!("{LLM_API_KEY_ENV} is not set"))?;
            if api_key.trim().is_empty() {
                bail!("{LLM_API_KEY_ENV} is empty");
            }
            Some(api_key)
        } else {
            None
        };
        let request = LlmTodoRequest {
            backend: self.settings.llm_backend,
            base_url: self.settings.llm_base_url.clone(),
            model: self.settings.llm_model.clone(),
            api_key,
            instructions,
            project,
            user_prompt: prompt,
            requested_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            codex_reasoning_effort: self.settings.codex_reasoning_effort,
            codex_fast_mode: self.settings.codex_fast_mode,
        };
        let (sender, receiver) = mpsc::channel();

        thread::spawn(move || {
            let result = request_llm_todo(&request).map_err(|error| error.to_string());
            let _ = sender.send(result);
        });

        self.llm_receiver = Some(receiver);
        if let Mode::Chat(form) = &mut self.mode {
            form.pending = true;
        }
        self.set_status_message(match self.settings.llm_backend {
            LlmBackend::CodexExec => "asking codex",
            LlmBackend::Api => "asking llm",
        });
        Ok(())
    }

    fn poll_llm_response(&mut self) {
        let Some(receiver) = &self.llm_receiver else {
            return;
        };

        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("llm worker disconnected".to_string()),
        };

        self.llm_receiver = None;
        if !matches!(self.mode, Mode::Chat(_)) {
            return;
        }

        match result {
            Ok(draft) => match llm_draft_to_task(draft, Utc::now()) {
                Ok(task) => self.open_llm_draft(task),
                Err(error) => {
                    self.mode = Mode::Normal;
                    self.log_chat_bar_error(&error.to_string());
                    self.set_status_message(short_error_message(&error));
                }
            },
            Err(error) => {
                self.mode = Mode::Normal;
                self.log_chat_bar_error(&error);
                self.set_status_message(short_error_message(&anyhow!(error)));
            }
        }
    }

    fn log_chat_bar_error(&self, error: &str) {
        let path = self.data_dir.join(CHAT_BAR_ERROR_LOG_FILE);
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };

        let _ = writeln!(file, "[{timestamp}] {error}\n");
    }

    fn open_llm_draft(&mut self, task: Task) {
        self.mode = Mode::Editing(EditForm::from_new_task(&task));
        self.set_status_message("review draft");
    }

    fn move_down(&mut self) -> Result<()> {
        match self.screen {
            Screen::Projects => {
                if self.project_index + 1 < self.projects.len() {
                    self.project_index += 1;
                    self.task_index = 0;
                }
            }
            Screen::ProjectDetail => self.move_visible_task(1),
            Screen::TaskDetail => self.detail_scroll = self.detail_scroll.saturating_add(1),
            Screen::Settings => {}
        }
        Ok(())
    }

    fn move_up(&mut self) -> Result<()> {
        match self.screen {
            Screen::Projects => {
                self.project_index = self.project_index.saturating_sub(1);
                self.task_index = 0;
            }
            Screen::ProjectDetail => self.move_visible_task(-1),
            Screen::TaskDetail => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            Screen::Settings => {}
        }
        Ok(())
    }

    fn open_selected(&mut self) -> Result<()> {
        match self.screen {
            Screen::Projects => {
                if self.current_project().is_some() {
                    self.screen = Screen::ProjectDetail;
                    self.task_index = 0;
                    self.detail_scroll = 0;
                    self.normalize_task_selection();
                }
            }
            Screen::ProjectDetail => {
                if let Some(task_index) = self.selected_task_index() {
                    self.task_index = task_index;
                    self.screen = Screen::TaskDetail;
                    self.detail_scroll = 0;
                }
            }
            Screen::TaskDetail => {}
            Screen::Settings => {}
        }
        Ok(())
    }

    fn toggle_selected_task_completion(&mut self) -> Result<()> {
        if self.screen != Screen::ProjectDetail {
            return Ok(());
        }

        let project_index = self.project_index;
        let task_index = self
            .selected_task_index()
            .ok_or_else(|| anyhow!("no task selected"))?;
        let updated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let project_file = self
            .projects
            .get_mut(project_index)
            .ok_or_else(|| anyhow!("no project selected"))?;
        let task = project_file
            .project
            .tasks
            .get_mut(task_index)
            .ok_or_else(|| anyhow!("no task selected"))?;

        let completed = task.completed_at.is_none();
        task.completed_at = completed.then_some(updated_at.clone());
        task.updated_at = Some(updated_at);
        save_project(project_file)?;

        self.task_index = task_index;
        self.normalize_task_selection();
        self.set_status_message(if completed {
            "task completed".to_string()
        } else {
            "task reopened".to_string()
        });
        Ok(())
    }

    fn toggle_hidden_tasks(&mut self) -> Result<()> {
        if self.screen != Screen::ProjectDetail {
            return Ok(());
        }

        self.show_hidden_tasks = !self.show_hidden_tasks;
        self.normalize_task_selection();
        self.set_status_message(if self.show_hidden_tasks {
            "showing hidden".to_string()
        } else {
            "hiding hidden".to_string()
        });
        Ok(())
    }

    fn start_editing_selected_task(&mut self) -> Result<()> {
        let task = self
            .current_task()
            .cloned()
            .ok_or_else(|| anyhow!("no task selected"))?;
        self.mode = Mode::Editing(EditForm::from_task(&task));
        Ok(())
    }

    fn start_delete_confirmation(&mut self) -> Result<()> {
        match self.screen {
            Screen::Projects => {
                if self.current_project().is_none() {
                    bail!("no project selected");
                }
                self.mode = Mode::ConfirmDelete(DeleteTarget::Project);
            }
            Screen::ProjectDetail | Screen::TaskDetail => {
                if self.current_task().is_none() {
                    bail!("no task selected");
                }
                self.mode = Mode::ConfirmDelete(DeleteTarget::Task);
            }
            Screen::Settings => {}
        }
        Ok(())
    }

    fn save_edit_form(&mut self) -> Result<()> {
        let (update, new_task) = match &self.mode {
            Mode::Editing(form) => (form.to_update()?, form.new_task.clone()),
            _ => return Ok(()),
        };
        let project_index = self.project_index;
        if let Some(mut task) = new_task {
            apply_task_update(&mut task, update, false);
            let project_file = self
                .projects
                .get_mut(project_index)
                .ok_or_else(|| anyhow!("no project selected"))?;
            project_file.project.tasks.push(task);
            save_project(project_file)?;

            self.task_index = project_file.project.tasks.len().saturating_sub(1);
            self.detail_scroll = 0;
            self.screen = Screen::ProjectDetail;
            self.mode = Mode::Normal;
            self.set_status_message("task added");
            return Ok(());
        }

        let task_index = self
            .selected_task_index()
            .ok_or_else(|| anyhow!("no task selected"))?;

        let project_file = self
            .projects
            .get_mut(project_index)
            .ok_or_else(|| anyhow!("no project selected"))?;
        let task = project_file
            .project
            .tasks
            .get_mut(task_index)
            .ok_or_else(|| anyhow!("no task selected"))?;

        apply_task_update(task, update, true);

        save_project(project_file)?;
        self.mode = Mode::Normal;
        self.set_status_message("task saved");
        Ok(())
    }

    fn delete_selected_project(&mut self) -> Result<()> {
        let project_index = self.project_index;
        let project = self
            .projects
            .get(project_index)
            .ok_or_else(|| anyhow!("no project selected"))?;
        let path = project.path.clone();

        fs::remove_file(&path)
            .with_context(|| format!("delete project file {}", path.display()))?;
        self.projects.remove(project_index);
        self.project_index = self
            .project_index
            .min(self.projects.len().saturating_sub(1));
        self.task_index = 0;
        self.detail_scroll = 0;
        self.screen = Screen::Projects;
        self.mode = Mode::Normal;
        self.set_status_message("project deleted");
        Ok(())
    }

    fn delete_selected_task(&mut self) -> Result<()> {
        let project_index = self.project_index;
        let task_index = self
            .selected_task_index()
            .ok_or_else(|| anyhow!("no task selected"))?;
        let project_file = self
            .projects
            .get_mut(project_index)
            .ok_or_else(|| anyhow!("no project selected"))?;

        project_file.project.tasks.remove(task_index);
        save_project(project_file)?;

        let task_count = project_file.project.tasks.len();
        self.task_index = self.task_index.min(task_count.saturating_sub(1));
        self.detail_scroll = 0;
        if self.screen == Screen::TaskDetail {
            self.screen = Screen::ProjectDetail;
        }
        self.normalize_task_selection();
        self.mode = Mode::Normal;
        self.set_status_message("task deleted");
        Ok(())
    }

    fn current_project(&self) -> Option<&ProjectFile> {
        self.projects.get(self.project_index)
    }

    fn current_task(&self) -> Option<&Task> {
        let project = self.current_project()?;
        project.project.tasks.get(self.selected_task_index()?)
    }

    fn selected_task_index(&self) -> Option<usize> {
        let project = self.current_project()?;
        if project.project.tasks.is_empty() {
            None
        } else if self.screen == Screen::ProjectDetail {
            let visible_tasks =
                visible_task_indices(&project.project.tasks, self.show_hidden_tasks);
            visible_tasks
                .iter()
                .copied()
                .find(|index| *index == self.task_index)
                .or_else(|| visible_tasks.first().copied())
        } else {
            Some(self.task_index.min(project.project.tasks.len() - 1))
        }
    }

    fn move_visible_task(&mut self, direction: isize) {
        let visible_tasks = self.visible_task_indices();
        if visible_tasks.is_empty() {
            self.task_index = 0;
            return;
        }

        let position = visible_tasks
            .iter()
            .position(|index| *index == self.task_index)
            .unwrap_or(0);
        let next_position = position
            .saturating_add_signed(direction)
            .min(visible_tasks.len() - 1);
        self.task_index = visible_tasks[next_position];
    }

    fn normalize_task_selection(&mut self) {
        let visible_tasks = self.visible_task_indices();
        if visible_tasks.is_empty() {
            self.task_index = 0;
            self.detail_scroll = 0;
            if self.screen == Screen::TaskDetail {
                self.screen = Screen::ProjectDetail;
            }
            return;
        }

        if !visible_tasks.contains(&self.task_index) {
            self.task_index = visible_tasks[0];
            self.detail_scroll = 0;
        }
    }

    fn visible_task_indices(&self) -> Vec<usize> {
        self.current_project()
            .map(|project| visible_task_indices(&project.project.tasks, self.show_hidden_tasks))
            .unwrap_or_default()
    }
}

#[derive(Debug)]
struct EditForm {
    fields: Vec<EditableField>,
    active: usize,
    new_task: Option<Task>,
}

#[derive(Debug)]
struct ChatForm {
    input: EditableField,
    pending: bool,
}

#[derive(Debug)]
struct SettingTextForm {
    field: SettingsField,
    input: EditableField,
}

#[derive(Debug, Clone)]
struct EditableField {
    label: &'static str,
    value: String,
    cursor: usize,
    scroll_top: usize,
    viewport_height: usize,
    multiline: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum EditAction {
    None,
    Save,
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
enum ChatAction {
    None,
    Submit(String),
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
enum SettingTextAction {
    None,
    Save,
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
struct TaskUpdate {
    title: String,
    content: String,
    labels: Vec<String>,
    branch: Option<String>,
    completed_at: Option<String>,
    due_at: Option<String>,
}

type LlmWorkerResult = std::result::Result<LlmTodoDraft, String>;

#[derive(Clone)]
struct LlmTodoRequest {
    backend: LlmBackend,
    base_url: String,
    model: String,
    api_key: Option<String>,
    instructions: String,
    project: Project,
    user_prompt: String,
    requested_at: String,
    codex_reasoning_effort: Option<CodexReasoningEffort>,
    codex_fast_mode: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LlmTodoDraft {
    title: String,
    content: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    due_at: Option<String>,
}

fn apply_task_update(task: &mut Task, update: TaskUpdate, touch_updated_at: bool) {
    task.title = update.title;
    task.content = update.content;
    task.labels = update.labels;
    task.branch = update.branch;
    task.completed_at = update.completed_at;
    task.due_at = update.due_at;

    if touch_updated_at {
        task.updated_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
    }
}

fn request_llm_todo(request: &LlmTodoRequest) -> Result<LlmTodoDraft> {
    match request.backend {
        LlmBackend::CodexExec => request_codex_exec_todo(request),
        LlmBackend::Api => request_api_todo(request),
    }
}

fn request_api_todo(request: &LlmTodoRequest) -> Result<LlmTodoDraft> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(LLM_REQUEST_TIMEOUT_SECS))
        .build()
        .context("build llm client")?;
    let response = client
        .post(chat_completions_url(&request.base_url)?)
        .bearer_auth(
            request
                .api_key
                .as_deref()
                .ok_or_else(|| anyhow!("{LLM_API_KEY_ENV} is not set"))?
                .trim(),
        )
        .json(&llm_request_body(request))
        .send()
        .context("send llm request")?;
    let status = response.status();
    let body = response.text().context("read llm response")?;

    if !status.is_success() {
        bail!("llm api returned {status}: {}", content_excerpt(&body, 160));
    }

    let value = serde_json::from_str::<Value>(&body).context("parse llm response")?;
    parse_chat_completion_draft(&value)
}

fn request_codex_exec_todo(request: &LlmTodoRequest) -> Result<LlmTodoDraft> {
    let args = codex_exec_args(request);
    let mut child = Command::new("codex")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn codex exec")?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("open codex stdin"))?;
        stdin
            .write_all(chat_bar_todo_prompt(request).as_bytes())
            .context("write codex prompt")?;
    }

    let output = child.wait_with_output().context("wait for codex exec")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "codex exec failed\ncommand: codex {}\nstderr:\n{}",
            args.join(" "),
            stderr.trim()
        );
    }
    if stdout.is_empty() {
        bail!("codex exec returned empty json stream");
    }

    parse_codex_json_final_message(&stdout)
        .with_context(|| format!("codex stdout: {}", content_excerpt(&stdout, 240)))
}

fn codex_exec_args(request: &LlmTodoRequest) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "--ephemeral".to_string(),
        "-s".to_string(),
        "read-only".to_string(),
    ];
    if let Some(effort) = request.codex_reasoning_effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={}", effort.label()));
    }
    if request.codex_fast_mode {
        args.push("-c".to_string());
        args.push("service_tier=\"fast\"".to_string());
    }
    args.extend([
        "--output-schema".to_string(),
        CHAT_BAR_OUTPUT_SCHEMA_FILE.to_string(),
        "--json".to_string(),
        "-".to_string(),
    ]);

    args
}

fn chat_completions_url(base_url: &str) -> Result<reqwest::Url> {
    let base_url = base_url.trim().trim_end_matches('/');
    let url = format!("{base_url}/chat/completions");

    reqwest::Url::parse(&url).with_context(|| format!("invalid llm base url {base_url}"))
}

fn validate_llm_base_url(base_url: &str) -> Result<()> {
    let url = chat_completions_url(base_url)?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        _ => bail!("llm base url must use http or https"),
    }
}

fn llm_request_body(request: &LlmTodoRequest) -> Value {
    serde_json::json!({
        "model": request.model.as_str(),
        "messages": [
            {
                "role": "system",
                "content": chat_bar_system_prompt(request),
            },
            {
                "role": "user",
                "content": request.user_prompt.as_str(),
            }
        ],
        "temperature": 0.2,
        "response_format": { "type": "json_object" },
    })
}

fn chat_bar_system_prompt(request: &LlmTodoRequest) -> String {
    format!(
        "{}\n\nTODO data instructions:\n{}\n\nCurrent time: {}\n\nSelected project:\n{}",
        CHAT_BAR_JSON_HARNESS,
        request.instructions.trim(),
        request.requested_at,
        llm_project_context(&request.project)
    )
}

fn chat_bar_todo_prompt(request: &LlmTodoRequest) -> String {
    format!(
        "{}\n\nUser prompt:\n{}",
        chat_bar_system_prompt(request),
        request.user_prompt.trim()
    )
}

fn llm_project_context(project: &Project) -> String {
    let task_titles = project
        .tasks
        .iter()
        .map(|task| format!("- {}", task.title))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Title: {}\nDescription: {}\nLabels: {}\nExisting todos:\n{}",
        project.title,
        project.description,
        format_labels(&project.labels),
        if task_titles.is_empty() {
            "- none".to_string()
        } else {
            task_titles
        }
    )
}

fn parse_chat_completion_draft(value: &Value) -> Result<LlmTodoDraft> {
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("llm response missing todo json"))?;

    parse_todo_draft_json(content)
}

fn parse_codex_json_final_message(stdout: &str) -> Result<LlmTodoDraft> {
    let mut final_message = None;

    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let value = serde_json::from_str::<Value>(line).context("parse codex json event")?;
        if value.pointer("/type").and_then(Value::as_str) == Some("item.completed")
            && value.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
            && let Some(text) = value.pointer("/item/text").and_then(Value::as_str)
        {
            final_message = Some(text.to_string());
        }
    }

    let final_message = final_message.ok_or_else(|| anyhow!("codex final message missing"))?;
    parse_todo_draft_json(&final_message)
}

fn parse_todo_draft_json(content: &str) -> Result<LlmTodoDraft> {
    serde_json::from_str::<LlmTodoDraft>(content.trim()).context("parse llm todo json")
}

fn llm_draft_to_task(draft: LlmTodoDraft, now: DateTime<Utc>) -> Result<Task> {
    let title = draft.title.trim().to_string();
    if title.is_empty() {
        bail!("draft title cannot be empty");
    }

    let content = draft.content.trim().to_string();
    if content.is_empty() {
        bail!("draft content cannot be empty");
    }

    Ok(Task {
        id: Uuid::now_v7().to_string(),
        title,
        content,
        labels: draft
            .labels
            .into_iter()
            .map(|label| label.trim().to_string())
            .filter(|label| !label.is_empty())
            .collect(),
        branch: draft.branch.and_then(|branch| parse_nullable_text(&branch)),
        created_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        updated_at: None,
        completed_at: None,
        due_at: match draft.due_at {
            Some(due_at) => parse_nullable_datetime(&due_at, "due_at")?,
            None => None,
        },
    })
}

impl EditForm {
    fn from_task(task: &Task) -> Self {
        Self {
            fields: vec![
                EditableField::single_line("Title", task.title.clone()),
                EditableField::multi_line("Content", task.content.clone()),
                EditableField::single_line("Labels", task.labels.join(", ")),
                EditableField::single_line("Branch", task.branch.clone().unwrap_or_default()),
                EditableField::single_line(
                    "Completed at",
                    task.completed_at.clone().unwrap_or_default(),
                ),
                EditableField::single_line("Due at", task.due_at.clone().unwrap_or_default()),
            ],
            active: 0,
            new_task: None,
        }
    }

    fn from_new_task(task: &Task) -> Self {
        let mut form = Self::from_task(task);
        form.new_task = Some(task.clone());
        form
    }

    fn handle_key(&mut self, key: KeyEvent) -> EditAction {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => EditAction::Cancel,
            (KeyCode::F(2), _) | (KeyCode::Char('s'), KeyModifiers::CONTROL) => EditAction::Save,
            (KeyCode::Tab, _) => {
                self.active = (self.active + 1) % self.fields.len();
                EditAction::None
            }
            (KeyCode::BackTab, _) => {
                self.active = if self.active == 0 {
                    self.fields.len() - 1
                } else {
                    self.active - 1
                };
                EditAction::None
            }
            (KeyCode::Enter, _) => {
                if self.fields[self.active].multiline {
                    self.fields[self.active].insert_char('\n');
                } else {
                    self.active = (self.active + 1) % self.fields.len();
                }
                EditAction::None
            }
            (KeyCode::Char(ch), KeyModifiers::NONE) | (KeyCode::Char(ch), KeyModifiers::SHIFT) => {
                self.fields[self.active].insert_char(ch);
                EditAction::None
            }
            (KeyCode::Backspace, _) => {
                self.fields[self.active].backspace();
                EditAction::None
            }
            (KeyCode::Delete, _) => {
                self.fields[self.active].delete();
                EditAction::None
            }
            (KeyCode::Left, _) => {
                self.fields[self.active].move_left();
                EditAction::None
            }
            (KeyCode::Right, _) => {
                self.fields[self.active].move_right();
                EditAction::None
            }
            (KeyCode::Up, _) => {
                self.fields[self.active].move_line_up();
                EditAction::None
            }
            (KeyCode::Down, _) => {
                self.fields[self.active].move_line_down();
                EditAction::None
            }
            (KeyCode::PageUp, _) => {
                self.fields[self.active].move_page_up();
                EditAction::None
            }
            (KeyCode::PageDown, _) => {
                self.fields[self.active].move_page_down();
                EditAction::None
            }
            (KeyCode::Home, _) => {
                self.fields[self.active].home();
                EditAction::None
            }
            (KeyCode::End, _) => {
                self.fields[self.active].end();
                EditAction::None
            }
            _ => EditAction::None,
        }
    }

    fn to_update(&self) -> Result<TaskUpdate> {
        let title = self.fields[0].value.trim().to_string();
        if title.is_empty() {
            bail!("title cannot be empty");
        }

        Ok(TaskUpdate {
            title,
            content: self.fields[1].value.clone(),
            labels: parse_labels(&self.fields[2].value),
            branch: parse_nullable_text(&self.fields[3].value),
            completed_at: parse_nullable_datetime(&self.fields[4].value, "completed_at")?,
            due_at: parse_nullable_datetime(&self.fields[5].value, "due_at")?,
        })
    }
}

impl ChatForm {
    fn new() -> Self {
        Self {
            input: EditableField::single_line("Prompt", String::new()),
            pending: false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ChatAction {
        if self.pending {
            return match key.code {
                KeyCode::Esc => ChatAction::Cancel,
                _ => ChatAction::None,
            };
        }

        match key.code {
            KeyCode::Esc => ChatAction::Cancel,
            KeyCode::Enter => ChatAction::Submit(self.input.value.clone()),
            _ => {
                handle_single_line_input(&mut self.input, key);
                ChatAction::None
            }
        }
    }
}

impl SettingTextForm {
    fn new(field: SettingsField, value: String) -> Self {
        Self {
            field,
            input: EditableField::single_line(field.label(), value),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> SettingTextAction {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => SettingTextAction::Cancel,
            (KeyCode::Enter, _)
            | (KeyCode::F(2), _)
            | (KeyCode::Char('s'), KeyModifiers::CONTROL) => SettingTextAction::Save,
            _ => {
                handle_single_line_input(&mut self.input, key);
                SettingTextAction::None
            }
        }
    }

    fn value(&self) -> Result<String> {
        let value = self.input.value.trim().to_string();
        if value.is_empty() {
            bail!("setting cannot be empty");
        }

        Ok(value)
    }
}

impl SettingsField {
    fn label(self) -> &'static str {
        match self {
            SettingsField::SyntaxTheme => "Theme",
            SettingsField::AppBackground => "App background",
            SettingsField::ModalBackground => "Modal background",
            SettingsField::TaskContentWrapCols => "Content wrap cols",
            SettingsField::LlmBackend => "LLM backend",
            SettingsField::CodexReasoningEffort => "Codex reasoning",
            SettingsField::CodexFastMode => "Codex fast",
            SettingsField::LlmBaseUrl => "LLM base URL",
            SettingsField::LlmModel => "LLM model",
        }
    }
}

fn handle_single_line_input(field: &mut EditableField, key: KeyEvent) {
    match key.code {
        KeyCode::Char(ch) if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            field.insert_char(ch)
        }
        KeyCode::Backspace => field.backspace(),
        KeyCode::Delete => field.delete(),
        KeyCode::Left => field.move_left(),
        KeyCode::Right => field.move_right(),
        KeyCode::Home => field.home(),
        KeyCode::End => field.end(),
        _ => {}
    }
}

impl EditableField {
    fn single_line(label: &'static str, value: String) -> Self {
        Self {
            label,
            cursor: value.chars().count(),
            scroll_top: 0,
            viewport_height: 1,
            value,
            multiline: false,
        }
    }

    fn multi_line(label: &'static str, value: String) -> Self {
        Self {
            label,
            cursor: value.chars().count(),
            scroll_top: 0,
            viewport_height: 1,
            value,
            multiline: true,
        }
    }

    fn insert_char(&mut self, ch: char) {
        if ch == '\n' && !self.multiline {
            return;
        }
        let byte_index = self.byte_index();
        self.value.insert(byte_index, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let current = self.byte_index();
        self.cursor -= 1;
        let previous = self.byte_index();
        self.value.replace_range(previous..current, "");
    }

    fn delete(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        let start = self.byte_index();
        self.cursor += 1;
        let end = self.byte_index();
        self.cursor -= 1;
        self.value.replace_range(start..end, "");
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }

    fn home(&mut self) {
        let (line, _) = self.cursor_line_column();
        self.cursor = self.char_index_for_line_column(line, 0);
    }

    fn end(&mut self) {
        let (line, _) = self.cursor_line_column();
        self.cursor = self.char_index_for_line_column(line, usize::MAX);
    }

    fn move_line_up(&mut self) {
        if !self.multiline {
            return;
        }

        let (line, column) = self.cursor_line_column();
        if line > 0 {
            self.cursor = self.char_index_for_line_column(line - 1, column);
        }
    }

    fn move_line_down(&mut self) {
        if !self.multiline {
            return;
        }

        let (line, column) = self.cursor_line_column();
        if line + 1 < self.line_count() {
            self.cursor = self.char_index_for_line_column(line + 1, column);
        }
    }

    fn move_page_up(&mut self) {
        if !self.multiline {
            return;
        }

        let (line, column) = self.cursor_line_column();
        self.cursor =
            self.char_index_for_line_column(line.saturating_sub(self.page_height()), column);
    }

    fn move_page_down(&mut self) {
        if !self.multiline {
            return;
        }

        let (line, column) = self.cursor_line_column();
        let last_line = self.line_count().saturating_sub(1);
        self.cursor =
            self.char_index_for_line_column((line + self.page_height()).min(last_line), column);
    }

    fn page_height(&self) -> usize {
        self.viewport_height.max(1)
    }

    fn display_value(&self, width: usize, height: usize) -> String {
        self.display_value_with_horizontal_start(width, height, self.horizontal_scroll_start(width))
    }

    fn display_preview(&self, width: usize, height: usize) -> String {
        self.display_value_with_horizontal_start(width, height, 0)
    }

    fn display_value_with_horizontal_start(
        &self,
        width: usize,
        height: usize,
        horizontal_start: usize,
    ) -> String {
        if width == 0 || height == 0 {
            return String::new();
        }

        if self.multiline {
            visible_multiline_value(
                &self.value,
                self.scroll_top,
                height,
                horizontal_start,
                width,
            )
        } else {
            visible_line_value(&self.value, horizontal_start, width)
        }
    }

    fn update_scroll(&mut self, viewport_height: usize) {
        if !self.multiline || viewport_height == 0 {
            return;
        }

        let max_scroll_top = self.line_count().saturating_sub(viewport_height);
        self.scroll_top = self.scroll_top.min(max_scroll_top);

        let cursor_line = self.cursor_line();
        if cursor_line <= self.scroll_top && self.scroll_top > 0 {
            self.scroll_top = cursor_line.saturating_sub(1);
        } else if cursor_line >= self.scroll_top + viewport_height {
            self.scroll_top = cursor_line + 1 - viewport_height;
        }
    }

    fn terminal_cursor_position(&self, area: Rect) -> Option<Position> {
        if area.width == 0 || area.height == 0 {
            return None;
        }

        let (line, column) = self.cursor_line_column();
        let visible_line = if self.multiline {
            line.checked_sub(self.scroll_top)?
        } else {
            0
        };

        if visible_line >= area.height as usize {
            return None;
        }

        let horizontal_start = self.horizontal_scroll_start(area.width as usize);
        let visible_column = column.saturating_sub(horizontal_start);
        let x = area.x + (visible_column as u16).min(area.width.saturating_sub(1));
        let y = area.y + visible_line as u16;

        Some(Position::new(x, y))
    }

    fn horizontal_scroll_start(&self, width: usize) -> usize {
        let (_, column) = self.cursor_line_column();
        // ponytail: char-based columns; switch to unicode-width if wide glyph editing matters.
        column.saturating_sub(width.saturating_sub(1))
    }

    fn cursor_line(&self) -> usize {
        self.value[..self.byte_index()]
            .chars()
            .filter(|ch| *ch == '\n')
            .count()
    }

    fn cursor_line_column(&self) -> (usize, usize) {
        let mut line = 0;
        let mut column = 0;

        for ch in self.value.chars().take(self.cursor) {
            if ch == '\n' {
                line += 1;
                column = 0;
            } else {
                column += 1;
            }
        }

        (line, column)
    }

    fn line_count(&self) -> usize {
        self.value.split('\n').count()
    }

    fn char_index_for_line_column(&self, target_line: usize, target_column: usize) -> usize {
        let mut current_line = 0;
        let mut current_column = 0;

        for (index, ch) in self.value.chars().enumerate() {
            if current_line == target_line && current_column == target_column {
                return index;
            }

            if ch == '\n' {
                if current_line == target_line {
                    return index;
                }
                current_line += 1;
                current_column = 0;
            } else {
                current_column += 1;
            }
        }

        self.value.chars().count()
    }

    fn byte_index(&self) -> usize {
        if self.cursor == self.value.chars().count() {
            self.value.len()
        } else {
            self.value
                .char_indices()
                .nth(self.cursor)
                .map(|(index, _)| index)
                .unwrap_or(self.value.len())
        }
    }
}

fn visible_multiline_value(
    value: &str,
    scroll_top: usize,
    height: usize,
    horizontal_start: usize,
    width: usize,
) -> String {
    if height == 0 {
        return String::new();
    }

    value
        .split('\n')
        .skip(scroll_top)
        .take(height)
        .map(|line| visible_line_value(line, horizontal_start, width))
        .collect::<Vec<_>>()
        .join("\n")
}

fn visible_line_value(value: &str, horizontal_start: usize, width: usize) -> String {
    value.chars().skip(horizontal_start).take(width).collect()
}

fn parse_labels(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_nullable_text(input: &str) -> Option<String> {
    let input = input.trim();
    (!input.is_empty()).then(|| input.to_string())
}

fn parse_nullable_datetime(input: &str, field: &str) -> Result<Option<String>> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }

    DateTime::parse_from_rfc3339(input)
        .map(|datetime| Some(datetime.to_rfc3339()))
        .with_context(|| format!("{field} must be an RFC3339 datetime or empty"))
}

fn visible_task_indices(tasks: &[Task], show_hidden: bool) -> Vec<usize> {
    let now = Utc::now();
    tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| {
            if show_hidden || !task_is_hidden(task, &now) {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

fn task_is_hidden(task: &Task, now: &DateTime<Utc>) -> bool {
    let Some(completed_at) = &task.completed_at else {
        return false;
    };
    let Ok(completed_at) = DateTime::parse_from_rfc3339(completed_at) else {
        return false;
    };

    completed_at.with_timezone(&Utc) < *now - ChronoDuration::days(HIDE_COMPLETED_AFTER_DAYS)
}

fn task_list_marker(task: &Task, now: &DateTime<Utc>) -> &'static str {
    if task_is_hidden(task, now) {
        "[hidden]"
    } else if task.completed_at.is_some() {
        "[x]"
    } else {
        "[ ]"
    }
}

fn draw_ui(frame: &mut Frame<'_>, app: &mut App) {
    frame.render_widget(
        Block::default().style(
            app.syntax_resources
                .ui_theme
                .style(app.settings.use_theme_app_background),
        ),
        frame.area(),
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    match app.screen {
        Screen::Projects => draw_projects_screen(frame, chunks[0], app),
        Screen::ProjectDetail => draw_project_detail_screen(frame, chunks[0], app),
        Screen::TaskDetail => draw_task_detail_screen(frame, chunks[0], app),
        Screen::Settings => draw_settings_screen(frame, chunks[0], app),
    }

    draw_footer(frame, chunks[1], app);

    let status_message = app.status_message_text().map(str::to_string);
    let delete_target = match &app.mode {
        Mode::ConfirmDelete(target) => Some(*target),
        _ => None,
    };

    if let Some(target) = delete_target {
        draw_delete_modal(frame, app, target);
    }

    if let Mode::Editing(form) = &mut app.mode {
        draw_edit_modal(
            frame,
            status_message.as_deref(),
            form,
            app.syntax_resources.ui_theme,
            app.settings.use_theme_modal_background,
        );
    }

    if let Mode::Chat(form) = &mut app.mode {
        draw_chat_bar(
            frame,
            form,
            app.syntax_resources.ui_theme,
            app.settings.use_theme_modal_background,
        );
    }

    if let Mode::EditingSetting(form) = &mut app.mode {
        draw_setting_text_modal(
            frame,
            form,
            app.syntax_resources.ui_theme,
            app.settings.use_theme_modal_background,
        );
    }
}

fn draw_projects_screen(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.syntax_resources.ui_theme;
    let style = theme.style(app.settings.use_theme_app_background);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);

    let items = if app.projects.is_empty() {
        vec![ListItem::new("No projects in data/")]
    } else {
        app.projects
            .iter()
            .map(|project_file| {
                let task_count = project_file.project.tasks.len();
                ListItem::new(Line::from(vec![
                    Span::styled(
                        &project_file.project.title,
                        Style::default().fg(theme.foreground),
                    ),
                    Span::raw(format!(" ({task_count})")),
                ]))
            })
            .collect()
    };
    let mut state = ListState::default();
    if !app.projects.is_empty() {
        state.select(Some(app.project_index));
    }
    let list = List::new(items)
        .style(style)
        .block(
            Block::default()
                .title(panel_title("Projects", theme))
                .borders(Borders::ALL),
        )
        .highlight_style(selector_bar_style(theme));
    frame.render_stateful_widget(list, chunks[0], &mut state);

    let preview = app
        .current_project()
        .map(|project| project_preview_lines(project, theme))
        .unwrap_or_else(|| vec![Line::from("Create a project JSON file in data/ to begin.")]);
    frame.render_widget(
        Paragraph::new(preview)
            .block(
                Block::default()
                    .title(panel_title("Project", theme))
                    .borders(Borders::ALL),
            )
            .style(style)
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn draw_project_detail_screen(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.syntax_resources.ui_theme;
    let style = theme.style(app.settings.use_theme_app_background);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(1)])
        .split(area);

    let Some(project_file) = app.current_project() else {
        frame.render_widget(Paragraph::new("No project selected").style(style), area);
        return;
    };

    frame.render_widget(
        Paragraph::new(project_preview_lines(project_file, theme))
            .block(
                Block::default()
                    .title(panel_title("Project Details", theme))
                    .borders(Borders::ALL),
            )
            .style(style)
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(chunks[1]);

    let tasks = &project_file.project.tasks;
    let now = Utc::now();
    let visible_tasks = visible_task_indices(tasks, app.show_hidden_tasks);
    let hidden_count = tasks
        .iter()
        .filter(|task| task_is_hidden(task, &now))
        .count();
    let items = if tasks.is_empty() {
        vec![ListItem::new("No todos")]
    } else if visible_tasks.is_empty() {
        vec![ListItem::new(
            "No visible todos. Press h to show hidden tasks.",
        )]
    } else {
        visible_tasks
            .iter()
            .map(|task_index| {
                let task = &tasks[*task_index];
                let hidden = task_is_hidden(task, &now);
                let marker = task_list_marker(task, &now);
                let style = if hidden {
                    Style::default().fg(theme.muted)
                } else {
                    Style::default().fg(theme.foreground)
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(&task.title, style),
                ]))
            })
            .collect()
    };
    let mut state = ListState::default();
    if !visible_tasks.is_empty() {
        state.select(
            visible_tasks
                .iter()
                .position(|task_index| *task_index == app.task_index)
                .or(Some(0)),
        );
    }
    let title = if app.show_hidden_tasks {
        "Todos (showing hidden)".to_string()
    } else if hidden_count > 0 {
        format!("Todos ({hidden_count} hidden)")
    } else {
        "Todos".to_string()
    };
    frame.render_stateful_widget(
        List::new(items)
            .style(style)
            .block(
                Block::default()
                    .title(panel_title(title, theme))
                    .borders(Borders::ALL),
            )
            .highlight_style(selector_bar_style(theme)),
        body[0],
        &mut state,
    );

    let detail = app
        .current_task()
        .map(|task| task_preview_lines(task, theme))
        .unwrap_or_else(|| vec![Line::from("No todo selected")]);
    frame.render_widget(
        Paragraph::new(detail)
            .block(
                Block::default()
                    .title(panel_title("Todo Preview", theme))
                    .borders(Borders::ALL),
            )
            .style(style)
            .wrap(Wrap { trim: false }),
        body[1],
    );
}

fn draw_task_detail_screen(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.syntax_resources.ui_theme;
    let style = theme.style(app.settings.use_theme_app_background);
    let Some(task) = app.current_task() else {
        frame.render_widget(Paragraph::new("No todo selected").style(style), area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(1)])
        .split(area);

    frame.render_widget(
        Paragraph::new(task_metadata_lines(task))
            .block(
                Block::default()
                    .title(panel_title("Todo Details", theme))
                    .borders(Borders::ALL),
            )
            .style(style)
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(render_content_lines(
            &task.content,
            &app.syntax_resources,
            app.settings.task_content_wrap_cols,
        ))
        .block(
            Block::default()
                .title(panel_title("Content", theme))
                .borders(Borders::ALL),
        )
        .style(style)
        .scroll((app.detail_scroll, 0)),
        chunks[1],
    );
}

fn draw_settings_screen(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.syntax_resources.ui_theme;
    let style = theme.style(app.settings.use_theme_app_background);
    let items = settings_items(app);
    let mut state = ListState::default();
    state.select(Some(app.settings_index.min(items.len().saturating_sub(1))));

    frame.render_stateful_widget(
        List::new(items)
            .style(style)
            .block(
                Block::default()
                    .title(panel_title("Settings", theme))
                    .borders(Borders::ALL),
            )
            .highlight_style(selector_bar_style(theme)),
        area,
        &mut state,
    );
}

fn settings_items(app: &App) -> Vec<ListItem<'static>> {
    let theme = app.syntax_resources.ui_theme;

    SETTINGS_FIELDS
        .iter()
        .map(|field| {
            let (label, value) = match field {
                SettingsField::SyntaxTheme => (field.label(), app.settings.syntax_theme.clone()),
                SettingsField::AppBackground => (
                    field.label(),
                    on_off(app.settings.use_theme_app_background).to_string(),
                ),
                SettingsField::ModalBackground => (
                    field.label(),
                    on_off(app.settings.use_theme_modal_background).to_string(),
                ),
                SettingsField::TaskContentWrapCols => (
                    field.label(),
                    app.settings.task_content_wrap_cols.to_string(),
                ),
                SettingsField::LlmBackend => {
                    (field.label(), app.settings.llm_backend.label().to_string())
                }
                SettingsField::CodexReasoningEffort => (
                    field.label(),
                    app.settings
                        .codex_reasoning_effort
                        .map(CodexReasoningEffort::label)
                        .unwrap_or("default")
                        .to_string(),
                ),
                SettingsField::CodexFastMode => (
                    field.label(),
                    if app.settings.codex_fast_mode {
                        "on"
                    } else {
                        "off"
                    }
                    .to_string(),
                ),
                SettingsField::LlmBaseUrl => (field.label(), app.settings.llm_base_url.clone()),
                SettingsField::LlmModel => (field.label(), app.settings.llm_model.clone()),
            };

            ListItem::new(Line::from(vec![
                Span::styled(format!("{label}: "), Style::default().fg(theme.muted)),
                Span::styled(value, Style::default().fg(theme.foreground)),
            ]))
        })
        .collect()
}

fn on_off(value: bool) -> &'static str {
    if value { "theme" } else { "terminal" }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(
        Paragraph::new(footer_line(app)).style(
            app.syntax_resources
                .ui_theme
                .style(app.settings.use_theme_app_background),
        ),
        area,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FooterAction {
    key: String,
    description: String,
}

fn footer_line(app: &App) -> Line<'static> {
    let theme = app.syntax_resources.ui_theme;
    let mut spans = Vec::new();

    if let Some(message) = app.status_message_text() {
        spans.push(Span::styled(
            message.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" | ", Style::default().fg(theme.muted)));
    }

    for (index, action) in footer_actions(app).into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(theme.muted)));
        }
        spans.push(Span::styled(action.key, footer_key_style(theme)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            action.description,
            Style::default().fg(theme.muted),
        ));
    }

    Line::from(spans)
}

fn footer_actions(app: &App) -> Vec<FooterAction> {
    match app.mode {
        Mode::Normal => match app.screen {
            Screen::Projects => vec![
                footer_action("Up/Down", "move"),
                footer_action("Enter", "open"),
                footer_action("c", "chat"),
                footer_action("s", "settings"),
                footer_action("d", "delete"),
                footer_action("q", "quit"),
            ],
            Screen::ProjectDetail => vec![
                footer_action("Up/Down", "move"),
                footer_action("Enter", "open"),
                footer_action("Space", "toggle done"),
                footer_action("c", "chat"),
                footer_action("e", "edit"),
                footer_action("d", "delete"),
                footer_action("s", "settings"),
                footer_action(
                    "h",
                    if app.show_hidden_tasks {
                        "hide hidden"
                    } else {
                        "show hidden"
                    },
                ),
                footer_action("q", "back"),
            ],
            Screen::TaskDetail => vec![
                footer_action("Up/Down", "scroll"),
                footer_action("c", "chat"),
                footer_action("e", "edit"),
                footer_action("d", "delete"),
                footer_action("s", "settings"),
                footer_action("q", "back"),
            ],
            Screen::Settings => vec![
                footer_action("Up/Down", "move"),
                footer_action("Left/Right", "change"),
                footer_action("q", "back"),
            ],
        },
        Mode::Chat(ref form) if form.pending => vec![footer_action("Esc", "cancel")],
        Mode::Chat(_) => vec![
            footer_action("Enter", "create draft"),
            footer_action("Esc", "cancel"),
        ],
        Mode::Editing(_) => vec![
            footer_action("Tab", "next"),
            footer_action("F2/Ctrl-S", "save"),
            footer_action("Esc", "cancel"),
        ],
        Mode::EditingSetting(_) => vec![
            footer_action("Enter/F2/Ctrl-S", "save"),
            footer_action("Esc", "cancel"),
        ],
        Mode::ConfirmDelete(_) => vec![
            footer_action("y/Enter", "confirm"),
            footer_action("n/Esc", "cancel"),
        ],
    }
}

fn footer_action(key: &str, description: &str) -> FooterAction {
    FooterAction {
        key: key.to_string(),
        description: description.to_string(),
    }
}

fn selector_bar_style(theme: UiTheme) -> Style {
    Style::default()
        .fg(theme.selection_text)
        .bg(theme.selection)
}

fn footer_key_style(theme: UiTheme) -> Style {
    Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

fn panel_title(title: impl Into<String>, theme: UiTheme) -> Line<'static> {
    Line::from(Span::styled(title.into(), footer_key_style(theme)))
}

fn draw_edit_modal(
    frame: &mut Frame<'_>,
    status_message: Option<&str>,
    form: &mut EditForm,
    theme: UiTheme,
    use_theme_background: bool,
) {
    let area = centered_rect(86, 86, frame.area());
    frame.render_widget(Clear, area);

    let style = theme.style(use_theme_background);
    let outer = Block::default()
        .title(panel_title("Edit Todo", theme))
        .borders(Borders::ALL)
        .style(style);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(inner);

    let mut cursor_position = None;

    for (index, field) in form.fields.iter_mut().enumerate() {
        let active = index == form.active;
        let border_style = if active {
            Style::default().fg(theme.accent)
        } else {
            Style::default()
        };
        let viewport_height = chunks[index].height.saturating_sub(2) as usize;
        field.viewport_height = viewport_height;
        if active {
            field.update_scroll(viewport_height);
        }
        let block = Block::default()
            .title(panel_title(field.label, theme))
            .borders(Borders::ALL)
            .border_style(border_style);
        let field_inner = block.inner(chunks[index]);
        if active {
            cursor_position = field.terminal_cursor_position(field_inner);
        }
        let viewport_width = field_inner.width as usize;
        let value = if active {
            field.display_value(viewport_width, viewport_height)
        } else {
            field.display_preview(viewport_width, viewport_height)
        };
        frame.render_widget(
            Paragraph::new(value).block(block).style(style),
            chunks[index],
        );
    }

    if let Some(position) = cursor_position {
        frame.set_cursor_position(position);
    }

    if let Some(message) = status_message {
        let message_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(message).style(style.fg(theme.error)),
            message_area,
        );
    }
}

fn draw_chat_bar(
    frame: &mut Frame<'_>,
    form: &mut ChatForm,
    theme: UiTheme,
    use_theme_background: bool,
) {
    let full = frame.area();
    let margin = 2.min(full.width / 2);
    let area = Rect {
        x: full.x + margin,
        y: full.y + full.height.saturating_sub(4),
        width: full.width.saturating_sub(margin * 2),
        height: 3.min(full.height),
    };
    frame.render_widget(Clear, area);

    let style = theme.style(use_theme_background);
    let border_style = if form.pending {
        Style::default().fg(theme.muted)
    } else {
        Style::default().fg(theme.accent)
    };
    let title = if form.pending {
        "New Todo: waiting"
    } else {
        "New Todo"
    };
    let block = Block::default()
        .title(panel_title(title, theme))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(style);
    let inner = block.inner(area);
    let value = if form.pending {
        "Creating draft...".to_string()
    } else {
        form.input.display_value(inner.width as usize, 1)
    };

    frame.render_widget(Paragraph::new(value).block(block).style(style), area);

    if !form.pending
        && let Some(position) = form.input.terminal_cursor_position(inner)
    {
        frame.set_cursor_position(position);
    }
}

fn draw_setting_text_modal(
    frame: &mut Frame<'_>,
    form: &mut SettingTextForm,
    theme: UiTheme,
    use_theme_background: bool,
) {
    let area = centered_rect(74, 18, frame.area());
    frame.render_widget(Clear, area);

    let style = theme.style(use_theme_background);
    let block = Block::default()
        .title(panel_title(form.field.label(), theme))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .style(style);
    let inner = block.inner(area);

    frame.render_widget(
        Paragraph::new(form.input.display_value(inner.width as usize, 1))
            .block(block)
            .style(style),
        area,
    );

    if let Some(position) = form.input.terminal_cursor_position(inner) {
        frame.set_cursor_position(position);
    }
}

fn draw_delete_modal(frame: &mut Frame<'_>, app: &App, target: DeleteTarget) {
    let theme = app.syntax_resources.ui_theme;
    let style = theme.style(app.settings.use_theme_modal_background);
    let area = centered_rect(58, 22, frame.area());
    frame.render_widget(Clear, area);

    let name = match target {
        DeleteTarget::Project => app
            .current_project()
            .map(|project_file| project_file.project.title.as_str())
            .unwrap_or("selected project"),
        DeleteTarget::Task => app
            .current_task()
            .map(|task| task.title.as_str())
            .unwrap_or("selected task"),
    };
    let prompt = match target {
        DeleteTarget::Project => format!("Delete project \"{name}\" and its JSON file?"),
        DeleteTarget::Task => format!("Delete todo \"{name}\"?"),
    };

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(prompt),
            Line::from(""),
            Line::from("Press y or Enter to confirm. Press n or Esc to cancel."),
        ])
        .block(
            Block::default()
                .title(panel_title("Confirm Delete", theme))
                .borders(Borders::ALL),
        )
        .style(style)
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn project_preview_lines(project_file: &ProjectFile, theme: UiTheme) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            project_file.project.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        property_line("File", format!("{}.json", project_file.file_stem), theme),
        property_line("Labels", format_labels(&project_file.project.labels), theme),
        property_line("Tasks", project_file.project.tasks.len().to_string(), theme),
        Line::from(""),
        Line::from(project_file.project.description.clone()),
    ]
}

fn task_preview_lines(task: &Task, theme: UiTheme) -> Vec<Line<'static>> {
    let now = Utc::now();
    let mut lines = vec![
        Line::from(vec![Span::styled(
            task.title.clone(),
            footer_key_style(theme),
        )]),
        property_line("Labels", format_labels(&task.labels), theme),
        property_line("Branch", task.branch.as_deref().unwrap_or("-"), theme),
        property_line(
            "Completed",
            task.completed_at.as_deref().unwrap_or("-"),
            theme,
        ),
        property_line("Due", task.due_at.as_deref().unwrap_or("-"), theme),
        Line::from(""),
    ];

    lines.push(property_label_line("Content", theme));
    lines.push(Line::from(content_excerpt(&task.content, 540)));

    if task_is_hidden(task, &now) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Hidden: completed more than 2 weeks ago",
            Style::default().fg(theme.muted),
        )));
    }

    lines
}

fn property_line(label: &'static str, value: impl Into<String>, theme: UiTheme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), footer_key_style(theme)),
        Span::raw(value.into()),
    ])
}

fn property_label_line(label: &'static str, theme: UiTheme) -> Line<'static> {
    Line::from(Span::styled(format!("{label}:"), footer_key_style(theme)))
}

fn task_metadata_lines(task: &Task) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            task.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(format!("Labels: {}", format_labels(&task.labels))),
        Line::from(format!("Branch: {}", task.branch.as_deref().unwrap_or("-"))),
        Line::from(format!("Created: {}", task.created_at)),
        Line::from(format!(
            "Updated: {}",
            task.updated_at.as_deref().unwrap_or("-")
        )),
        Line::from(format!(
            "Completed: {}",
            task.completed_at.as_deref().unwrap_or("-")
        )),
        Line::from(format!("Due: {}", task.due_at.as_deref().unwrap_or("-"))),
    ]
}

fn content_excerpt(content: &str, max_chars: usize) -> String {
    let cleaned = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ")
        .replace("**", "")
        .replace(['_', '`'], "");

    if cleaned.is_empty() {
        return "-".to_string();
    }

    let char_count = cleaned.chars().count();
    if char_count <= max_chars {
        return cleaned;
    }

    let keep = max_chars.saturating_sub(3);
    format!("{}...", cleaned.chars().take(keep).collect::<String>())
}

fn format_labels(labels: &[String]) -> String {
    if labels.is_empty() {
        "-".to_string()
    } else {
        labels.join(", ")
    }
}

fn render_content_lines(
    content: &str,
    syntax_resources: &SyntaxResources,
    wrap_cols: usize,
) -> Vec<Line<'static>> {
    let mut rendered = Vec::new();

    for block in split_content_blocks(content) {
        match block {
            ContentBlock::Text(text) => {
                rendered.extend(render_text_lines(
                    &text,
                    wrap_cols,
                    syntax_resources.ui_theme,
                ));
            }
            ContentBlock::Code { language, code } => {
                rendered.extend(render_code_block(&code, &language, syntax_resources))
            }
        }
    }

    if rendered.is_empty() {
        rendered.push(Line::from(""));
    }

    rendered
}

fn render_text_lines(text: &str, wrap_cols: usize, theme: UiTheme) -> Vec<Line<'static>> {
    let lines = text.split('\n').collect::<Vec<_>>();
    let mut rendered = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if let Some((table, next_index)) = parse_table(&lines, index) {
            rendered.extend(render_table(table, theme));
            index = next_index;
        } else {
            rendered.extend(render_inline_lines_wrapped(lines[index], wrap_cols, theme));
            index += 1;
        }
    }

    rendered
}

fn parse_table(lines: &[&str], start: usize) -> Option<(Vec<Vec<String>>, usize)> {
    let header = parse_table_row(*lines.get(start)?)?;
    let separator = parse_table_row(*lines.get(start + 1)?)?;
    if !is_table_separator(&separator) {
        return None;
    }

    let mut rows = vec![header];
    let mut index = start + 2;
    while let Some(row) = lines.get(index).and_then(|line| parse_table_row(line)) {
        rows.push(row);
        index += 1;
    }

    Some((rows, index))
}

fn parse_table_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }

    let cells = trimmed
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect::<Vec<_>>();

    (cells.len() >= 2).then_some(cells)
}

fn is_table_separator(cells: &[String]) -> bool {
    cells.iter().all(|cell| {
        let marker = cell.trim().trim_matches(':');
        marker.len() >= 3 && marker.chars().all(|ch| ch == '-')
    })
}

fn render_table(rows: Vec<Vec<String>>, theme: UiTheme) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let mut lines = Vec::new();

    for (index, row) in rows.iter().enumerate() {
        let header = index == 0;
        lines.push(render_table_row(row, &widths, theme, header));
        if header {
            lines.push(render_table_rule(&widths, theme));
        }
    }

    lines
}

fn render_table_row(
    row: &[String],
    widths: &[usize],
    theme: UiTheme,
    header: bool,
) -> Line<'static> {
    let mut spans = Vec::new();
    let style = if header {
        footer_key_style(theme)
    } else {
        Style::default()
    };

    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default().fg(theme.muted)));
        }
        let cell = row.get(index).map(String::as_str).unwrap_or("");
        spans.push(Span::styled(pad_table_cell(cell, *width), style));
    }

    Line::from(spans)
}

fn render_table_rule(widths: &[usize], theme: UiTheme) -> Line<'static> {
    let mut spans = Vec::new();

    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default().fg(theme.muted)));
        }
        spans.push(Span::styled(
            "-".repeat(*width),
            Style::default().fg(theme.muted),
        ));
    }

    Line::from(spans)
}

fn pad_table_cell(cell: &str, width: usize) -> String {
    format!(
        "{}{}",
        cell,
        " ".repeat(width.saturating_sub(cell.chars().count()))
    )
}

#[derive(Debug, PartialEq, Eq)]
enum ContentBlock {
    Text(String),
    Code { language: String, code: String },
}

fn split_content_blocks(content: &str) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let mut remaining = content;

    while let Some(open_index) = remaining.find("```") {
        let before = &remaining[..open_index];
        if !before.is_empty() {
            blocks.push(ContentBlock::Text(before.to_string()));
        }

        let after_open = &remaining[open_index + 3..];
        let Some(close_index) = after_open.find("```") else {
            let (language, code) = parse_fence_body(after_open);
            blocks.push(ContentBlock::Code { language, code });
            return blocks;
        };

        let fenced = &after_open[..close_index];
        let (language, code) = parse_fence_body(fenced);
        blocks.push(ContentBlock::Code { language, code });
        remaining = &after_open[close_index + 3..];
    }

    if !remaining.is_empty() {
        blocks.push(ContentBlock::Text(remaining.to_string()));
    }

    blocks
}

fn parse_fence_body(fenced: &str) -> (String, String) {
    let fenced = fenced.strip_prefix('\n').unwrap_or(fenced);

    if let Some(newline_index) = fenced.find('\n') {
        return (
            fenced[..newline_index]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string(),
            fenced[newline_index + 1..].trim_matches('\n').to_string(),
        );
    }

    let trimmed = fenced.trim();
    if let Some((language, code)) = trimmed.split_once(char::is_whitespace) {
        (language.to_string(), code.trim().to_string())
    } else {
        ("".to_string(), trimmed.to_string())
    }
}

fn render_code_block(
    code: &str,
    language: &str,
    syntax_resources: &SyntaxResources,
) -> Vec<Line<'static>> {
    let syntax = if language.is_empty() {
        syntax_resources.syntax_set.find_syntax_plain_text()
    } else {
        syntax_resources
            .syntax_set
            .find_syntax_by_token(language)
            .or_else(|| {
                syntax_resources
                    .syntax_set
                    .find_syntax_by_extension(language)
            })
            .unwrap_or_else(|| syntax_resources.syntax_set.find_syntax_plain_text())
    };
    let mut highlighter = HighlightLines::new(syntax, &syntax_resources.theme);
    let code_lines: Vec<&str> = code.trim_end_matches('\n').lines().collect();

    if code_lines.is_empty() {
        return vec![Line::from(Span::styled(
            "",
            Style::default().fg(syntax_resources.ui_theme.inline_code),
        ))];
    }

    code_lines
        .into_iter()
        .map(
            |line| match highlighter.highlight_line(line, &syntax_resources.syntax_set) {
                Ok(ranges) => Line::from(
                    ranges
                        .into_iter()
                        .map(|(style, text)| {
                            Span::styled(text.to_string(), syntect_to_ratatui(style))
                        })
                        .collect::<Vec<_>>(),
                ),
                Err(_) => plain_code_line(line, syntax_resources.ui_theme),
            },
        )
        .collect()
}

fn plain_code_line(line: &str, theme: UiTheme) -> Line<'static> {
    Line::from(Span::styled(
        line.to_string(),
        Style::default().fg(theme.inline_code),
    ))
}

fn syntect_to_ratatui(style: SyntectStyle) -> Style {
    let mut modifiers = Modifier::empty();

    if style.font_style.contains(FontStyle::BOLD) {
        modifiers |= Modifier::BOLD;
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        modifiers |= Modifier::ITALIC;
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        modifiers |= Modifier::UNDERLINED;
    }

    Style::default()
        .fg(Color::Rgb(
            style.foreground.r,
            style.foreground.g,
            style.foreground.b,
        ))
        .add_modifier(modifiers)
}

#[derive(Debug, Clone)]
struct InlineToken {
    text: String,
    style: Style,
    atomic: bool,
}

fn render_inline_lines_wrapped(line: &str, width: usize, theme: UiTheme) -> Vec<Line<'static>> {
    wrap_inline_tokens(parse_inline_tokens(line, theme), width)
}

fn parse_inline_tokens(line: &str, theme: UiTheme) -> Vec<InlineToken> {
    let mut tokens = Vec::new();
    let mut remaining = line;

    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix("**")
            && let Some(end) = rest.find("**")
        {
            tokens.push(InlineToken {
                text: rest[..end].to_string(),
                style: Style::default().add_modifier(Modifier::BOLD),
                atomic: false,
            });
            remaining = &rest[end + 2..];
            continue;
        }

        if let Some(rest) = remaining.strip_prefix('`')
            && let Some(end) = rest.find('`')
        {
            tokens.push(InlineToken {
                text: rest[..end].to_string(),
                style: Style::default().fg(theme.inline_code),
                atomic: true,
            });
            remaining = &rest[end + 1..];
            continue;
        }

        if let Some(rest) = remaining.strip_prefix('_')
            && let Some(end) = rest.find('_')
        {
            tokens.push(InlineToken {
                text: rest[..end].to_string(),
                style: Style::default().add_modifier(Modifier::ITALIC),
                atomic: false,
            });
            remaining = &rest[end + 1..];
            continue;
        }

        let next = next_marker_index(remaining).unwrap_or(remaining.len());
        let split_at = if next == 0 {
            remaining
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(remaining.len())
        } else {
            next
        };
        tokens.push(InlineToken {
            text: remaining[..split_at].to_string(),
            style: Style::default(),
            atomic: false,
        });
        remaining = &remaining[split_at..];
    }

    tokens
}

fn wrap_inline_tokens(tokens: Vec<InlineToken>, width: usize) -> Vec<Line<'static>> {
    if tokens.is_empty() {
        return vec![Line::from("")];
    }

    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;
    let mut pending_space = false;

    for token in tokens {
        if token.atomic {
            push_wrapped_word(
                &mut lines,
                &mut current,
                &mut current_width,
                &mut pending_space,
                token.text,
                token.style,
                width,
            );
            continue;
        }

        for word in split_words_and_spaces(&token.text) {
            if word.chars().all(char::is_whitespace) {
                if current_width > 0 {
                    pending_space = true;
                }
            } else {
                push_wrapped_word(
                    &mut lines,
                    &mut current,
                    &mut current_width,
                    &mut pending_space,
                    word,
                    token.style,
                    width,
                );
            }
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(current));
    }

    lines
}

fn push_wrapped_word(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    pending_space: &mut bool,
    word: String,
    style: Style,
    width: usize,
) {
    let word_width = word.chars().count();
    let space_width = usize::from(*pending_space && *current_width > 0);

    if *current_width > 0 && *current_width + space_width + word_width > width {
        lines.push(Line::from(std::mem::take(current)));
        *current_width = 0;
        *pending_space = false;
    }

    if *pending_space && *current_width > 0 {
        current.push(Span::raw(" "));
        *current_width += 1;
    }

    current.push(Span::styled(word, style));
    *current_width += word_width;
    *pending_space = false;
}

fn split_words_and_spaces(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_is_space = None;

    for ch in text.chars() {
        let is_space = ch.is_whitespace();
        if current_is_space.is_some_and(|space| space != is_space) {
            parts.push(std::mem::take(&mut current));
        }
        current.push(ch);
        current_is_space = Some(is_space);
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

fn next_marker_index(input: &str) -> Option<usize> {
    ["**", "`", "_"]
        .into_iter()
        .filter_map(|marker| input.find(marker))
        .min()
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_app(data_dir: &Path) -> Result<App> {
        App::new(data_dir.to_path_buf(), PathBuf::from(SETTINGS_FILE))
    }

    fn test_syntax_resources() -> SyntaxResources {
        SyntaxResources::new(&Settings::default()).unwrap()
    }

    fn test_ui_theme() -> UiTheme {
        test_syntax_resources().ui_theme
    }

    fn render_test_content_lines(content: &str) -> Vec<Line<'static>> {
        render_content_lines(
            content,
            &test_syntax_resources(),
            DEFAULT_TASK_CONTENT_WRAP_COLS,
        )
    }

    fn sample_project() -> Project {
        Project {
            title: "Example".to_string(),
            description: "Project description".to_string(),
            labels: vec!["project".to_string()],
            tasks: vec![Task {
                id: "0197f27f-83b0-7000-8000-000000000001".to_string(),
                title: "Do the thing".to_string(),
                content: "Content with **bold**, _italic_, and `code`.".to_string(),
                labels: vec!["task".to_string()],
                branch: None,
                created_at: "2026-06-25T10:10:06+02:00".to_string(),
                updated_at: None,
                completed_at: None,
                due_at: None,
            }],
        }
    }

    fn task_with_id(id: &str) -> Task {
        let mut task = sample_project().tasks.remove(0);
        task.id = id.to_string();
        task
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn sample_llm_request() -> LlmTodoRequest {
        LlmTodoRequest {
            backend: LlmBackend::CodexExec,
            base_url: DEFAULT_LLM_BASE_URL.to_string(),
            model: DEFAULT_LLM_MODEL.to_string(),
            api_key: None,
            instructions: "Write useful todos.".to_string(),
            project: sample_project(),
            user_prompt: "Customer asked for follow-up during the call.".to_string(),
            requested_at: "2026-06-25T10:00:00Z".to_string(),
            codex_reasoning_effort: None,
            codex_fast_mode: false,
        }
    }

    #[test]
    fn loads_projects() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join("real.json"),
            serde_json::to_string_pretty(&sample_project())?,
        )?;

        let projects = load_projects(dir.path())?;

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].file_stem, "real");
        assert_eq!(projects[0].project.title, "Example");
        Ok(())
    }

    #[test]
    fn loads_tasks_without_branch_as_null() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join("real.json"),
            serde_json::json!({
                "title": "Example",
                "description": "Project description",
                "labels": [],
                "tasks": [{
                    "id": "0197f27f-83b0-7000-8000-000000000001",
                    "title": "Do the thing",
                    "content": "",
                    "labels": [],
                    "created_at": "2026-06-25T10:10:06+02:00",
                    "updated_at": null,
                    "completed_at": null,
                    "due_at": null
                }]
            })
            .to_string(),
        )?;

        let projects = load_projects(dir.path())?;

        assert_eq!(projects[0].project.tasks[0].branch, None);
        Ok(())
    }

    #[test]
    fn loads_statusline_timeout_from_settings_file() -> Result<()> {
        let dir = tempdir()?;
        let settings_path = dir.path().join("settings.json");
        fs::write(
            &settings_path,
            serde_json::json!({
                "statusline_message_timeout_ms": 1250,
                "syntax_theme": "InspiredGitHub",
                "syntax_theme_folder": "custom-themes"
            })
            .to_string(),
        )?;

        let settings = load_settings(&settings_path)?;

        assert_eq!(settings.statusline_message_timeout_ms, 1250);
        assert_eq!(settings.syntax_theme, "InspiredGitHub");
        assert_eq!(settings.syntax_theme_folder, "custom-themes");
        assert!(settings.use_theme_app_background);
        assert!(settings.use_theme_modal_background);
        assert_eq!(settings.task_content_wrap_cols, 120);
        assert_eq!(settings.llm_backend, LlmBackend::CodexExec);
        assert_eq!(settings.codex_reasoning_effort, None);
        assert!(!settings.codex_fast_mode);
        assert_eq!(settings.llm_base_url, DEFAULT_LLM_BASE_URL);
        assert_eq!(settings.llm_model, DEFAULT_LLM_MODEL);
        assert_eq!(
            settings.statusline_message_timeout(),
            Duration::from_millis(1250)
        );
        Ok(())
    }

    #[test]
    fn loads_theme_options_from_theme_folder() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join("zeta.tmTheme"), "")?;
        fs::write(dir.path().join("alpha.tmTheme"), "")?;
        fs::write(dir.path().join("ignored.txt"), "")?;
        let settings = Settings {
            syntax_theme_folder: dir.path().display().to_string(),
            ..Settings::default()
        };

        let options = load_theme_options(&settings)?;

        assert_eq!(options, vec!["alpha", "zeta"]);
        Ok(())
    }

    #[test]
    fn validates_llm_base_url() {
        assert!(validate_llm_base_url("https://api.openai.com/v1").is_ok());
        assert!(validate_llm_base_url("file:///tmp/nope").is_err());
    }

    #[test]
    fn settings_screen_cycles_llm_backend() -> Result<()> {
        let dir = tempdir()?;
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir)?;
        fs::write(
            data_dir.join("project.json"),
            serde_json::to_string_pretty(&sample_project())?,
        )?;
        let settings_path = dir.path().join("settings.json");

        let mut app = App::new(data_dir, settings_path.clone())?;
        app.open_settings()?;
        app.settings_index = SETTINGS_FIELDS
            .iter()
            .position(|field| *field == SettingsField::LlmBackend)
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

        let saved = load_settings(&settings_path)?;

        assert_eq!(saved.llm_backend, LlmBackend::Api);
        Ok(())
    }

    #[test]
    fn settings_screen_configures_codex_reasoning_and_fast_mode() -> Result<()> {
        let dir = tempdir()?;
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir)?;
        fs::write(
            data_dir.join("project.json"),
            serde_json::to_string_pretty(&sample_project())?,
        )?;
        let settings_path = dir.path().join("settings.json");

        let mut app = App::new(data_dir, settings_path.clone())?;
        app.open_settings()?;
        app.settings_index = SETTINGS_FIELDS
            .iter()
            .position(|field| *field == SettingsField::CodexReasoningEffort)
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.settings_index = SETTINGS_FIELDS
            .iter()
            .position(|field| *field == SettingsField::CodexFastMode)
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

        let saved = load_settings(&settings_path)?;

        assert_eq!(
            saved.codex_reasoning_effort,
            Some(CodexReasoningEffort::Minimal)
        );
        assert!(saved.codex_fast_mode);
        Ok(())
    }

    #[test]
    fn chat_bar_todo_prompt_includes_harness_context_and_user_prompt() {
        let prompt = chat_bar_todo_prompt(&sample_llm_request());

        assert!(prompt.contains("chat bar JSON harness"));
        assert!(prompt.contains("Return only one valid JSON object."));
        assert!(prompt.contains("Do not include Markdown"));
        assert!(prompt.contains("Write useful todos."));
        assert!(prompt.contains("Selected project:"));
        assert!(prompt.contains("Title: Example"));
        assert!(prompt.contains("Customer asked for follow-up"));
    }

    #[test]
    fn codex_exec_args_are_ephemeral_schema_backed_and_jsonl() {
        let args = codex_exec_args(&sample_llm_request());

        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--ephemeral".to_string()));
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-s" && pair[1] == "read-only")
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("model_reasoning_effort"))
        );
        assert!(!args.iter().any(|arg| arg.contains("service_tier")));
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--output-schema" && pair[1] == "schemas/chat-bar-todo.schema.json"
        }));
        assert!(args.contains(&"--json".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn codex_exec_args_include_reasoning_and_fast_overrides() {
        let mut request = sample_llm_request();
        request.codex_reasoning_effort = Some(CodexReasoningEffort::High);
        request.codex_fast_mode = true;

        let args = codex_exec_args(&request);

        assert!(args.contains(&"model_reasoning_effort=high".to_string()));
        assert!(args.contains(&"service_tier=\"fast\"".to_string()));
    }

    #[test]
    fn chat_bar_output_schema_exists_and_is_json() -> Result<()> {
        let schema = fs::read_to_string(CHAT_BAR_OUTPUT_SCHEMA_FILE)?;
        let schema = serde_json::from_str::<Value>(&schema)?;

        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&Value::from("title"))
        );
        Ok(())
    }

    #[test]
    fn theme_style_uses_background_only_when_enabled() {
        let theme = test_ui_theme();

        assert_eq!(theme.style(true).bg, Some(theme.background));
        assert_eq!(theme.style(false).bg, None);
    }

    #[test]
    fn settings_screen_saves_theme_and_background_choices() -> Result<()> {
        let dir = tempdir()?;
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir)?;
        fs::write(
            data_dir.join("project.json"),
            serde_json::to_string_pretty(&sample_project())?,
        )?;
        let settings_path = dir.path().join("settings.json");
        fs::write(
            &settings_path,
            serde_json::json!({
                "statusline_message_timeout_ms": 3000,
                "syntax_theme": "base16-black-metal-dark-funeral",
                "syntax_theme_folder": "themes",
                "use_theme_app_background": true,
                "use_theme_modal_background": true
            })
            .to_string(),
        )?;

        let mut app = App::new(data_dir, settings_path.clone())?;
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

        let saved = load_settings(&settings_path)?;

        assert_eq!(app.screen, Screen::Settings);
        assert_ne!(saved.syntax_theme, "base16-black-metal-dark-funeral");
        assert!(!saved.use_theme_app_background);
        assert!(!saved.use_theme_modal_background);
        assert_eq!(saved.task_content_wrap_cols, 130);
        Ok(())
    }

    #[test]
    fn syntax_resources_use_configured_builtin_theme() -> Result<()> {
        let settings = Settings {
            syntax_theme: "InspiredGitHub".to_string(),
            ..Settings::default()
        };

        SyntaxResources::new(&settings)?;

        Ok(())
    }

    #[test]
    fn status_messages_expire_after_configured_timeout() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;

        let mut app = test_app(dir.path())?;
        app.settings.statusline_message_timeout_ms = 10;
        app.set_status_message("project reloaded");
        app.status_message.as_mut().unwrap().created_at =
            Instant::now() - Duration::from_millis(11);

        app.expire_status_message();

        assert!(app.status_message_text().is_none());
        Ok(())
    }

    #[test]
    fn chat_bar_errors_are_logged_to_data_dir() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;
        let app = test_app(dir.path())?;

        app.log_chat_bar_error("codex exec failed\nstderr:\nnope");

        let log = fs::read_to_string(dir.path().join(CHAT_BAR_ERROR_LOG_FILE))?;
        assert!(log.contains("codex exec failed"));
        assert!(log.contains("nope"));
        Ok(())
    }

    #[test]
    fn footer_styles_status_and_keybind_keys() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;

        let mut app = test_app(dir.path())?;
        let theme = app.syntax_resources.ui_theme;
        app.set_status_message("project reloaded");
        let line = footer_line(&app);

        assert!(line.spans.iter().any(|span| {
            span.content.as_ref() == "project reloaded" && span.style.fg == Some(theme.accent)
        }));
        assert!(line.spans.iter().any(|span| {
            span.content.as_ref() == "Up/Down"
                && span.style.fg == Some(theme.accent)
                && span.style.bg.is_none()
        }));
        Ok(())
    }

    #[test]
    fn c_opens_chat_bar_for_selected_project() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;

        let mut app = test_app(dir.path())?;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));

        assert!(matches!(app.mode, Mode::Chat(_)));
        let actions = footer_actions(&app);
        assert_eq!(actions[0], footer_action("Enter", "create draft"));
        Ok(())
    }

    #[test]
    fn ui_theme_uses_selected_syntax_theme_colors() -> Result<()> {
        let resources = SyntaxResources::new(&Settings {
            syntax_theme: "base16-black-metal-venom".to_string(),
            ..Settings::default()
        })?;

        assert_eq!(resources.ui_theme.background, Color::Rgb(0, 0, 0));
        assert_eq!(resources.ui_theme.accent, Color::Rgb(252, 48, 46));
        assert_eq!(resources.ui_theme.selection, Color::Rgb(51, 51, 51));
        assert_eq!(resources.ui_theme.inline_code, Color::Rgb(248, 247, 242));
        Ok(())
    }

    #[test]
    fn parses_chat_completion_todo_draft() -> Result<()> {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "content": r#"{"title":"Call customer","content":"Ask for the missing invoice number.","labels":["call","billing"],"branch":null,"due_at":null}"#
                }
            }]
        });

        let draft = parse_chat_completion_draft(&response)?;

        assert_eq!(draft.title, "Call customer");
        assert_eq!(draft.labels, vec!["call", "billing"]);
        assert_eq!(draft.branch, None);
        Ok(())
    }

    #[test]
    fn parses_direct_todo_draft_json_and_rejects_fenced_output() -> Result<()> {
        let direct = parse_todo_draft_json(
            r#"{"title":"Direct","content":"Plain final message.","labels":[],"branch":null,"due_at":null}"#,
        )?;
        let fenced = parse_todo_draft_json(
            "```json\n{\"title\":\"Fenced\",\"content\":\"Fenced final message.\",\"labels\":[],\"branch\":null,\"due_at\":null}\n```",
        );

        assert_eq!(direct.title, "Direct");
        assert!(fenced.is_err());
        Ok(())
    }

    #[test]
    fn parses_codex_jsonl_final_agent_message() -> Result<()> {
        let stdout = r#"{"type":"thread.started","thread_id":"1"}
{"type":"item.completed","item":{"type":"agent_message","text":"{\"title\":\"Codex\",\"content\":\"JSONL final message.\",\"labels\":[],\"branch\":null,\"due_at\":null}"}}
{"type":"turn.completed"}"#;

        let draft = parse_codex_json_final_message(stdout)?;

        assert_eq!(draft.title, "Codex");
        Ok(())
    }

    #[test]
    fn rejects_invalid_chat_completion_todo_draft() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "content": r#"{"content":"missing title"}"#
                }
            }]
        });

        let error = parse_chat_completion_draft(&response)
            .unwrap_err()
            .to_string();

        assert!(error.contains("parse llm todo json"));
    }

    #[test]
    fn rejects_todo_draft_json_with_extra_fields() {
        let error = parse_todo_draft_json(
            r#"{"id":"nope","title":"Direct","content":"Plain final message.","labels":[],"branch":null,"due_at":null}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("parse llm todo json"));
    }

    #[test]
    fn converts_llm_draft_to_new_task_with_app_owned_fields() -> Result<()> {
        let task = llm_draft_to_task(
            LlmTodoDraft {
                title: "  Follow up ".to_string(),
                content: " Capture the decision. ".to_string(),
                labels: vec![" meeting ".to_string(), "".to_string()],
                branch: Some(" ".to_string()),
                due_at: Some("2026-06-26T09:00:00+02:00".to_string()),
            },
            DateTime::parse_from_rfc3339("2026-06-25T10:00:00+02:00")?.with_timezone(&Utc),
        )?;

        assert_eq!(Uuid::parse_str(&task.id)?.get_version_num(), 7);
        assert_eq!(task.title, "Follow up");
        assert_eq!(task.content, "Capture the decision.");
        assert_eq!(task.labels, vec!["meeting"]);
        assert_eq!(task.branch, None);
        assert_eq!(task.created_at, "2026-06-25T08:00:00Z");
        assert_eq!(task.updated_at, None);
        assert_eq!(task.completed_at, None);
        assert_eq!(task.due_at.as_deref(), Some("2026-06-26T09:00:00+02:00"));
        Ok(())
    }

    #[test]
    fn rejects_malformed_project_json() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join("broken.json"), "{ nope")?;

        let error = load_projects(dir.path()).unwrap_err().to_string();

        assert!(error.contains("parse project file"));
        Ok(())
    }

    #[test]
    fn renders_inline_content_styles() {
        let theme = test_ui_theme();
        let lines = render_test_content_lines("A **bold** _italic_ `code` line");
        let spans = &lines[0].spans;

        assert!(
            spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::ITALIC))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.style.fg == Some(theme.inline_code))
        );
    }

    #[test]
    fn renders_plain_and_language_code_fences() {
        let plain = render_test_content_lines("```text\nplain\n```");
        let highlighted = render_test_content_lines("```c\nint main(void) { return 0; }\n```");

        assert_eq!(plain[0].spans[0].content, "plain");
        assert!(
            highlighted[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("int main")
        );
    }

    #[test]
    fn renders_markdown_tables_in_content() {
        let theme = test_ui_theme();
        let lines = render_content_lines(
            "before\n| Name | State |\n| --- | --- |\n| Thing | Done |\nafter",
            &test_syntax_resources(),
            DEFAULT_TASK_CONTENT_WRAP_COLS,
        );

        assert_eq!(line_text(&lines[0]), "before");
        assert_eq!(line_text(&lines[1]), "Name   State");
        assert_eq!(line_text(&lines[2]), "-----  -----");
        assert_eq!(line_text(&lines[3]).trim_end(), "Thing  Done");
        assert_eq!(line_text(&lines[4]), "after");
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.accent));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme.muted));
    }

    #[test]
    fn wraps_task_detail_content_at_120_columns() {
        let content = "word ".repeat(80);
        let lines = render_test_content_lines(&content);

        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| line_text(line).chars().count() <= DEFAULT_TASK_CONTENT_WRAP_COLS)
        );
    }

    #[test]
    fn wraps_task_detail_content_at_configured_columns() {
        let lines = render_content_lines("one two three four five", &test_syntax_resources(), 9);

        assert_eq!(line_text(&lines[0]), "one two");
        assert_eq!(line_text(&lines[1]), "three");
    }

    #[test]
    fn inline_code_moves_to_next_line_before_wrapping_mid_statement() {
        let prefix = "x".repeat(DEFAULT_TASK_CONTENT_WRAP_COLS - 4);
        let code = "let value = call();";
        let lines = render_test_content_lines(&format!("{prefix} `{code}` after"));

        assert_eq!(line_text(&lines[0]), prefix);
        assert!(line_text(&lines[1]).starts_with(code));
        assert_eq!(lines[1].spans[0].content, code);
        assert_eq!(
            lines[1].spans[0].style.fg,
            Some(test_ui_theme().inline_code)
        );
    }

    #[test]
    fn fenced_code_blocks_render_on_newlines_when_embedded_in_prose() {
        let lines = render_test_content_lines("before ```rust let value = call();``` after");

        assert_eq!(line_text(&lines[0]), "before");
        assert!(line_text(&lines[1]).contains("let value = call();"));
        assert_eq!(line_text(&lines[2]), "after");
    }

    #[test]
    fn content_excerpt_removes_supported_markers_and_code_fences() {
        let excerpt = content_excerpt(
            "**Keep** _this_ `code`.\n```c\nint main(void) { return 0; }\n```",
            200,
        );

        assert_eq!(excerpt, "Keep this code. int main(void) { return 0; }");
    }

    #[test]
    fn task_preview_includes_content_excerpt() {
        let task = &sample_project().tasks[0];
        let theme = test_ui_theme();
        let preview = task_preview_lines(task, theme);

        assert_eq!(line_text(&preview[5]), "");
        assert_eq!(line_text(&preview[6]), "Content:");
        assert_eq!(
            line_text(&preview[7]),
            "Content with bold, italic, and code."
        );
        assert_eq!(preview[0].spans[0].style.fg, Some(theme.accent));
        assert_eq!(preview[1].spans[0].style.fg, Some(theme.accent));
        assert_eq!(preview[2].spans[0].style.fg, Some(theme.accent));
        assert_eq!(preview[6].spans[0].style.fg, Some(theme.accent));
    }

    #[test]
    fn old_completed_tasks_are_hidden_until_toggled() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        let mut project = sample_project();
        let mut old_completed = task_with_id("0197f27f-83b0-7000-8000-000000000002");
        old_completed.title = "Old completed".to_string();
        old_completed.completed_at = Some(
            (Utc::now() - ChronoDuration::days(HIDE_COMPLETED_AFTER_DAYS + 1))
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        let mut recent_completed = task_with_id("0197f27f-83b0-7000-8000-000000000003");
        recent_completed.title = "Recent completed".to_string();
        recent_completed.completed_at = Some(
            (Utc::now() - ChronoDuration::days(HIDE_COMPLETED_AFTER_DAYS - 1))
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        project.tasks = vec![old_completed, recent_completed];
        fs::write(&path, serde_json::to_string_pretty(&project)?)?;

        let mut app = test_app(dir.path())?;
        app.screen = Screen::ProjectDetail;
        app.normalize_task_selection();

        assert_eq!(app.visible_task_indices(), vec![1]);
        assert_eq!(app.task_index, 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));

        assert!(app.show_hidden_tasks);
        assert_eq!(app.visible_task_indices(), vec![0, 1]);
        Ok(())
    }

    #[test]
    fn hidden_tasks_have_distinct_marker_and_preview_note() {
        let now = Utc::now();
        let mut task = task_with_id("0197f27f-83b0-7000-8000-000000000002");
        task.completed_at = Some(
            (now - ChronoDuration::days(HIDE_COMPLETED_AFTER_DAYS + 1))
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        let preview = task_preview_lines(&task, test_ui_theme())
            .into_iter()
            .flat_map(|line| line.spans.into_iter())
            .map(|span| span.content.to_string())
            .collect::<String>();

        assert_eq!(task_list_marker(&task, &Utc::now()), "[hidden]");
        assert!(preview.contains("Hidden: completed more than 2 weeks ago"));
    }

    #[test]
    fn space_toggles_selected_task_completion_in_project_detail() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;

        let mut app = test_app(dir.path())?;
        app.screen = Screen::ProjectDetail;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        let task = &app.projects[0].project.tasks[0];
        assert!(task.completed_at.is_some());
        assert!(task.updated_at.is_some());

        let saved = load_projects(dir.path())?;
        let saved_task = &saved[0].project.tasks[0];
        assert!(saved_task.completed_at.is_some());
        assert_eq!(saved_task.completed_at, saved_task.updated_at);

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        let task = &app.projects[0].project.tasks[0];
        assert!(task.completed_at.is_none());
        assert!(task.updated_at.is_some());

        let saved = load_projects(dir.path())?;
        let saved_task = &saved[0].project.tasks[0];
        assert!(saved_task.completed_at.is_none());
        assert!(saved_task.updated_at.is_some());
        Ok(())
    }

    #[test]
    fn edit_update_keeps_created_at_out_of_form_and_validates_dates() -> Result<()> {
        let task = sample_project().tasks.remove(0);
        let mut form = EditForm::from_task(&task);

        assert_eq!(form.fields.len(), 6);
        assert!(!form.fields.iter().any(|field| field.label == "Created at"));

        form.fields[0].value = "New title".to_string();
        form.fields[2].value = "one, two, , three".to_string();
        form.fields[3].value = "feature/example".to_string();
        form.fields[4].value = "2026-06-25T12:00:00+02:00".to_string();
        let update = form.to_update()?;

        assert_eq!(update.title, "New title");
        assert_eq!(update.labels, vec!["one", "two", "three"]);
        assert_eq!(update.branch.as_deref(), Some("feature/example"));
        assert_eq!(
            update.completed_at.as_deref(),
            Some("2026-06-25T12:00:00+02:00")
        );

        form.fields[3].value = "   ".to_string();
        assert_eq!(form.to_update()?.branch, None);

        form.fields[5].value = "not a date".to_string();
        let error = form.to_update().unwrap_err().to_string();
        assert!(error.contains("due_at must be an RFC3339 datetime"));
        Ok(())
    }

    #[test]
    fn multiline_edit_field_scrolls_to_keep_cursor_visible() {
        let value = (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut field = EditableField::multi_line("Content", value);

        field.update_scroll(5);
        let visible = field.display_value(80, 5);

        assert_eq!(field.scroll_top, 15);
        assert!(visible.starts_with("line 15"));
        assert!(visible.contains("line 19"));
        assert!(!visible.contains("line 0"));
    }

    #[test]
    fn multiline_edit_field_scrolls_when_cursor_hits_top() {
        let value = (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut field = EditableField::multi_line("Content", value);
        field.scroll_top = 10;
        field.cursor = field.char_index_for_line_column(10, 0);

        field.update_scroll(5);
        let visible = field.display_value(80, 5);

        assert_eq!(field.scroll_top, 9);
        assert!(visible.starts_with("line 9"));
    }

    #[test]
    fn edit_field_reports_terminal_cursor_position() {
        let mut field = EditableField::multi_line("Content", "zero\none\ntwo".to_string());
        field.scroll_top = 1;
        field.cursor = field.char_index_for_line_column(2, 2);

        let position = field
            .terminal_cursor_position(Rect::new(5, 7, 20, 4))
            .expect("cursor should be visible");

        assert_eq!(position.x, 7);
        assert_eq!(position.y, 8);
    }

    #[test]
    fn long_single_line_edit_field_scrolls_with_cursor() {
        let field = EditableField::single_line("Title", "0123456789".to_string());

        assert_eq!(field.display_value(5, 1), "6789");
        assert_eq!(
            field
                .terminal_cursor_position(Rect::new(10, 4, 5, 1))
                .expect("cursor should be visible"),
            Position::new(14, 4)
        );
    }

    #[test]
    fn home_and_end_move_to_current_line_bounds() {
        let mut field = EditableField::multi_line("Content", "short\nmuch longer\nend".to_string());

        field.cursor = field.char_index_for_line_column(1, 5);
        field.home();
        assert_eq!(field.cursor_line_column(), (1, 0));

        field.end();
        assert_eq!(field.cursor_line_column(), (1, "much longer".len()));
    }

    #[test]
    fn page_up_and_down_move_by_visible_page_height() {
        let value = (0..10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut field = EditableField::multi_line("Content", value);
        field.viewport_height = 4;
        field.cursor = field.char_index_for_line_column(8, 2);

        field.move_page_up();
        assert_eq!(field.cursor_line_column(), (4, 2));

        field.move_page_down();
        assert_eq!(field.cursor_line_column(), (8, 2));

        field.move_page_down();
        assert_eq!(field.cursor_line_column(), (9, 2));
    }

    #[test]
    fn multiline_edit_field_moves_cursor_between_lines() {
        let mut field = EditableField::multi_line("Content", "short\nmuch longer\nend".to_string());

        field.cursor = 0;
        field.move_line_down();
        assert_eq!(field.cursor_line_column(), (1, 0));

        field.move_right();
        field.move_right();
        field.move_line_down();
        assert_eq!(field.cursor_line_column(), (2, 2));

        field.move_line_up();
        assert_eq!(field.cursor_line_column(), (1, 2));
    }

    #[test]
    fn saving_task_updates_updated_at_but_not_created_at() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        let project = sample_project();
        fs::write(&path, serde_json::to_string_pretty(&project)?)?;

        let mut app = test_app(dir.path())?;
        app.screen = Screen::TaskDetail;
        app.mode = Mode::Editing(EditForm::from_task(&app.current_task().unwrap().clone()));
        if let Mode::Editing(form) = &mut app.mode {
            form.fields[0].value = "Updated title".to_string();
            form.fields[3].value = "feature/example".to_string();
        }

        app.save_edit_form()?;

        let saved = load_projects(dir.path())?;
        let task = &saved[0].project.tasks[0];
        assert_eq!(task.title, "Updated title");
        assert_eq!(task.branch.as_deref(), Some("feature/example"));
        assert_eq!(task.created_at, "2026-06-25T10:10:06+02:00");
        assert!(task.updated_at.is_some());
        Ok(())
    }

    #[test]
    fn saving_llm_draft_appends_new_task() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;

        let mut app = test_app(dir.path())?;
        let task = llm_draft_to_task(
            LlmTodoDraft {
                title: "Draft title".to_string(),
                content: "Draft content".to_string(),
                labels: vec!["llm".to_string()],
                branch: None,
                due_at: None,
            },
            Utc::now(),
        )?;

        app.open_llm_draft(task);
        if let Mode::Editing(form) = &mut app.mode {
            form.fields[0].value = "Reviewed title".to_string();
        }
        app.save_edit_form()?;

        let saved = load_projects(dir.path())?;
        let task = saved[0].project.tasks.last().unwrap();
        assert_eq!(saved[0].project.tasks.len(), 2);
        assert_eq!(task.title, "Reviewed title");
        assert_eq!(task.content, "Draft content");
        assert_eq!(task.labels, vec!["llm"]);
        assert!(task.updated_at.is_none());
        assert_eq!(app.screen, Screen::ProjectDetail);
        assert_eq!(app.task_index, 1);
        Ok(())
    }

    #[test]
    fn deletes_task_and_project_file() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;

        let mut app = test_app(dir.path())?;
        app.delete_selected_task()?;
        assert!(app.projects[0].project.tasks.is_empty());

        app.delete_selected_project()?;
        assert!(!path.exists());
        assert!(app.projects.is_empty());
        Ok(())
    }

    #[test]
    fn watcher_is_enabled_only_for_open_project_views() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        fs::write(&path, serde_json::to_string_pretty(&sample_project())?)?;

        let mut app = test_app(dir.path())?;
        app.sync_project_watcher();
        assert!(app.project_watcher.is_none());

        app.screen = Screen::ProjectDetail;
        app.sync_project_watcher();
        assert!(app.project_watcher.is_some());

        app.screen = Screen::Projects;
        app.sync_project_watcher();
        assert!(app.project_watcher.is_none());
        Ok(())
    }

    #[test]
    fn reload_current_project_from_disk_picks_up_added_todos() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        let mut project = sample_project();
        fs::write(&path, serde_json::to_string_pretty(&project)?)?;

        let mut app = test_app(dir.path())?;
        app.screen = Screen::ProjectDetail;

        let mut added = project.tasks[0].clone();
        added.id = "0197f27f-83b0-7000-8000-000000000002".to_string();
        added.title = "Added by LLM".to_string();
        added.content = "Fresh external content".to_string();
        project.tasks.push(added);
        fs::write(&path, serde_json::to_string_pretty(&project)?)?;

        app.reload_current_project_from_disk()?;

        assert_eq!(app.projects[0].project.tasks.len(), 2);
        assert_eq!(app.projects[0].project.tasks[1].title, "Added by LLM");
        assert_eq!(app.task_index, 0);
        Ok(())
    }

    #[test]
    fn reload_current_project_from_disk_preserves_selection_by_id() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("project.json");
        let mut project = sample_project();
        let mut second = project.tasks[0].clone();
        second.id = "0197f27f-83b0-7000-8000-000000000002".to_string();
        second.title = "Second task".to_string();
        second.created_at = "2026-06-25T11:10:06+02:00".to_string();
        project.tasks.push(second);
        fs::write(&path, serde_json::to_string_pretty(&project)?)?;

        let mut app = test_app(dir.path())?;
        app.screen = Screen::ProjectDetail;
        app.task_index = 1;

        project.tasks.swap(0, 1);
        project.tasks[0].title = "Renamed second task".to_string();
        fs::write(&path, serde_json::to_string_pretty(&project)?)?;

        app.reload_current_project_from_disk()?;

        assert_eq!(app.task_index, 0);
        assert_eq!(app.current_task().unwrap().title, "Renamed second task");
        Ok(())
    }
}
