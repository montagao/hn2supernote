use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, NaiveDate, Utc};
use clap::Parser;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode, size,
};
use crossterm::{execute, queue};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const STATE_ORDER: &[StateKind] = &[
    StateKind::Backlog,
    StateKind::Todo,
    StateKind::Started,
    StateKind::Done,
];
const PRIORITY_ORDER: &[Priority] = &[
    Priority::Urgent,
    Priority::High,
    Priority::Medium,
    Priority::Low,
    Priority::None,
];
const FRAMES: &[&str] = &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
const CARD_HEIGHT: u16 = 6;
const LIST_GAP: u16 = 1;
const LIST_MARK_WIDTH: u16 = 3;
const LIST_PRIORITY_WIDTH: u16 = 2;
const LIST_KEY_WIDTH: u16 = 10;
const LIST_STATE_WIDTH: u16 = 15;
const LIST_LABELS_MAX_WIDTH: u16 = 17;
const LIST_TITLE_MIN_WIDTH: u16 = 10;
const LIST_DUE_WIDTH: u16 = 7;
const LIST_UPDATED_WIDTH: u16 = 9;
const MOUSE_SCROLL_LINES: isize = 3;
const MAX_COALESCED_NAV_KEYS: usize = 256;
const BUSINESS_CONTEXT: &str = include_str!("business_context.md");
const BG: Color = Color::Rgb { r: 9, g: 12, b: 17 };
const BG_RAISE: Color = Color::Rgb {
    r: 13,
    g: 17,
    b: 24,
};
const CELL_BG: Color = Color::Rgb {
    r: 15,
    g: 19,
    b: 27,
};
const LINE: Color = Color::Rgb {
    r: 35,
    g: 42,
    b: 54,
};
const PAPER: Color = Color::Rgb {
    r: 207,
    g: 194,
    b: 165,
};
const DIM: Color = Color::Rgb {
    r: 102,
    g: 101,
    b: 111,
};
const DIMMER: Color = Color::Rgb {
    r: 70,
    g: 72,
    b: 84,
};
const ACCENT: Color = Color::Rgb {
    r: 91,
    g: 113,
    b: 202,
};
const TEXT: Color = Color::Rgb {
    r: 205,
    g: 174,
    b: 132,
};
const AMBER: Color = Color::Rgb {
    r: 211,
    g: 151,
    b: 54,
};
const RED: Color = Color::Rgb {
    r: 224,
    g: 105,
    b: 91,
};
const GREEN: Color = Color::Rgb {
    r: 101,
    g: 203,
    b: 142,
};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, env = "PLANE_BASE_URL", hide_env_values = true)]
    base_url: Option<String>,
    #[arg(long, env = "PLANE_API_URL", hide_env_values = true)]
    api_url: Option<String>,
    #[arg(long, env = "PLANE_API_KEY", hide_env_values = true)]
    api_key: String,
    #[arg(long, env = "PLANE_WORKSPACE_SLUG", default_value = "translatemom")]
    workspace: String,
    #[arg(long, default_value = "Product,iOS,Growth")]
    projects: String,
    #[arg(long, default_value_t = 100)]
    per_page: usize,
    #[arg(long)]
    check_api: bool,
    #[arg(long, env = "PLANE_TUI_AGENT_BACKEND")]
    agent_backend: Option<String>,
    #[arg(long, env = "PLANE_TUI_CODEX_BIN", default_value = "codex")]
    codex_bin: String,
    #[arg(long, env = "PLANE_TUI_CLAUDE_BIN", default_value = "claude")]
    claude_bin: String,
    #[arg(long, env = "PLANE_TUI_CLAUDE_MODEL")]
    claude_model: Option<String>,
    #[arg(long, env = "PLANE_TUI_CLAUDE_EFFORT")]
    claude_effort: Option<String>,
    #[arg(long, env = "PLANE_TUI_REPO_DIR")]
    repo_dir: Option<String>,
    #[arg(long, env = "PLANE_TUI_CONTEXT_FILE")]
    context_file: Option<String>,
    #[arg(long, env = "PLANE_TUI_AUTO_REFRESH", default_value_t = 5)]
    auto_refresh: u64,
    #[arg(long, env = "PLANE_TUI_WIP_LIMIT", default_value_t = 2)]
    wip_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentBackend {
    Codex,
    Claude,
}

impl AgentBackend {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Clone)]
struct Config {
    base_url: String,
    api_key: String,
    workspace: String,
    wanted_projects: Vec<String>,
    per_page: usize,
    check_api: bool,
    agent_backend: AgentBackend,
    codex_bin: String,
    claude_bin: String,
    claude_model: String,
    claude_effort: String,
    repo_dir: Option<String>,
    context_file: Option<String>,
    auto_refresh_mins: u64,
    wip_limit: usize,
}

impl Config {
    fn agent_bin(&self) -> &str {
        match self.agent_backend {
            AgentBackend::Codex => &self.codex_bin,
            AgentBackend::Claude => &self.claude_bin,
        }
    }

    fn agent_summary(&self) -> String {
        match self.agent_backend {
            AgentBackend::Codex => format!("codex ({})", self.codex_bin),
            AgentBackend::Claude => format!(
                "claude · model {} · effort {}",
                self.claude_model, self.claude_effort
            ),
        }
    }
}

impl Config {
    fn from_args() -> Result<Self> {
        let mut args = Args::parse();
        if args.workspace == "translatemom" {
            if let Ok(value) = std::env::var("PLANE_WORKPLACE_SLUG") {
                args.workspace = value;
            }
        }
        let base_url = args
            .base_url
            .or(args.api_url)
            .ok_or_else(|| anyhow!("set PLANE_BASE_URL or PLANE_API_URL"))?
            .trim_end_matches('/')
            .to_owned();
        let mut wanted_projects = args
            .projects
            .split(',')
            .map(|part| part.trim().to_lowercase())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        for project in remembered_projects(&args.workspace).unwrap_or_default() {
            if !wanted_projects.contains(&project) {
                wanted_projects.push(project);
            }
        }
        let saved = saved_agent_prefs().unwrap_or_default();
        let agent_backend = match args.agent_backend.as_deref() {
            Some(value) => AgentBackend::parse(value)
                .ok_or_else(|| anyhow!("invalid --agent-backend {value:?} (codex or claude)"))?,
            None => saved
                .backend
                .as_deref()
                .and_then(AgentBackend::parse)
                .unwrap_or(AgentBackend::Claude),
        };
        Ok(Self {
            base_url,
            api_key: args.api_key,
            workspace: args.workspace,
            wanted_projects,
            per_page: args.per_page.clamp(10, 200),
            check_api: args.check_api,
            agent_backend,
            codex_bin: args.codex_bin,
            claude_bin: args.claude_bin,
            claude_model: args
                .claude_model
                .or(saved.model)
                .unwrap_or_else(|| "claude-fable-5".to_owned()),
            claude_effort: args
                .claude_effort
                .or(saved.effort)
                .unwrap_or_else(|| "high".to_owned()),
            repo_dir: args.repo_dir,
            context_file: args.context_file,
            auto_refresh_mins: args.auto_refresh,
            wip_limit: args.wip_limit,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ApiProject {
    id: String,
    name: String,
    identifier: String,
    #[serde(default)]
    archived_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiState {
    id: String,
    name: String,
    group: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiLabel {
    id: String,
    name: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiItem {
    id: String,
    name: String,
    sequence_id: i64,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    state_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    label_ids: Vec<String>,
    #[serde(default)]
    label_details: Vec<ApiLabel>,
    #[serde(default)]
    description_html: Option<String>,
    #[serde(default)]
    target_date: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    #[serde(default)]
    archived_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiComment {
    #[serde(default)]
    comment_html: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct PlaneClient {
    http: Client,
    config: Config,
}

impl PlaneClient {
    fn new(config: Config) -> Self {
        Self {
            http: Client::new(),
            config,
        }
    }

    fn api_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn request_get(&self, path_or_url: &str) -> Result<Value> {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_owned()
        } else {
            self.api_url(path_or_url)
        };
        let response = self
            .http
            .get(&url)
            .header("X-API-Key", &self.config.api_key)
            .header("Content-Type", "application/json")
            .send()
            .with_context(|| format!("GET {url}"))?;
        Self::decode(response, "GET", &url)
    }

    fn request_json(&self, method: &str, path: &str, body: Value) -> Result<Value> {
        let url = self.api_url(path);
        let builder = match method {
            "POST" => self.http.post(&url),
            "PATCH" => self.http.patch(&url),
            _ => bail!("unsupported method {method}"),
        };
        let response = builder
            .header("X-API-Key", &self.config.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .with_context(|| format!("{method} {url}"))?;
        Self::decode(response, method, &url)
    }

    fn decode(response: reqwest::blocking::Response, method: &str, url: &str) -> Result<Value> {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        if !status.is_success() {
            bail!("{method} {url} failed: {status} {text}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("{method} {url} returned invalid JSON"))
    }

    fn list_all<T>(&self, path: &str) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut results = Vec::new();
        let mut next_url = Some(self.api_url(path));
        let mut seen = BTreeSet::new();
        while let Some(url) = next_url {
            if !seen.insert(url.clone()) {
                bail!("pagination loop for {path}");
            }
            let raw = self.request_get(&url)?;
            if raw.is_array() {
                let mut batch: Vec<T> = serde_json::from_value(raw)?;
                results.append(&mut batch);
                break;
            }
            let page_results = raw
                .get("results")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let mut batch: Vec<T> = serde_json::from_value(page_results)?;
            results.append(&mut batch);
            let next = raw.get("next").and_then(Value::as_str).map(str::to_owned);
            next_url = next.map(|next| {
                if next.starts_with("http") {
                    next
                } else {
                    self.api_url(&next)
                }
            });
            let has_next_page = raw
                .get("next_page_results")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if next_url.is_none() && has_next_page {
                if let Some(cursor) = raw.get("next_cursor").and_then(Value::as_str) {
                    let sep = if path.contains('?') { '&' } else { '?' };
                    next_url = Some(self.api_url(&format!("{path}{sep}cursor={cursor}")));
                }
            }
        }
        Ok(results)
    }

    fn projects(&self) -> Result<Vec<ApiProject>> {
        self.list_all(&format!(
            "/api/v1/workspaces/{}/projects/",
            self.config.workspace
        ))
    }

    fn states(&self, project_id: &str) -> Result<Vec<ApiState>> {
        self.list_all(&format!(
            "/api/v1/workspaces/{}/projects/{project_id}/states/",
            self.config.workspace
        ))
    }

    fn labels(&self, project_id: &str) -> Result<Vec<ApiLabel>> {
        self.list_all(&format!(
            "/api/v1/workspaces/{}/projects/{project_id}/labels/",
            self.config.workspace
        ))
    }

    fn create_label(&self, project_id: &str, body: Value) -> Result<ApiLabel> {
        let raw = self.request_json(
            "POST",
            &format!(
                "/api/v1/workspaces/{}/projects/{project_id}/labels/",
                self.config.workspace
            ),
            body,
        )?;
        serde_json::from_value(raw).context("create label response")
    }

    fn create_project(&self, body: Value) -> Result<ApiProject> {
        let raw = self.request_json(
            "POST",
            &format!("/api/v1/workspaces/{}/projects/", self.config.workspace),
            body,
        )?;
        serde_json::from_value(raw).context("create project response")
    }

    fn work_items(&self, project_id: &str, per_page: usize) -> Result<Vec<ApiItem>> {
        self.list_all(&format!(
            "/api/v1/workspaces/{}/projects/{project_id}/work-items/?per_page={per_page}",
            self.config.workspace
        ))
    }

    fn update_work_item(&self, project_id: &str, item_id: &str, body: Value) -> Result<Value> {
        self.request_json(
            "PATCH",
            &format!(
                "/api/v1/workspaces/{}/projects/{project_id}/work-items/{item_id}/",
                self.config.workspace
            ),
            body,
        )
    }

    fn create_work_item(&self, project_id: &str, body: Value) -> Result<Value> {
        self.request_json(
            "POST",
            &format!(
                "/api/v1/workspaces/{}/projects/{project_id}/work-items/",
                self.config.workspace
            ),
            body,
        )
    }

    fn create_comment(&self, project_id: &str, item_id: &str, body: Value) -> Result<Value> {
        self.request_json(
            "POST",
            &format!(
                "/api/v1/workspaces/{}/projects/{project_id}/work-items/{item_id}/comments/",
                self.config.workspace
            ),
            body,
        )
    }

    fn list_comments(&self, project_id: &str, item_id: &str) -> Result<Vec<ApiComment>> {
        self.list_all(&format!(
            "/api/v1/workspaces/{}/projects/{project_id}/work-items/{item_id}/comments/",
            self.config.workspace
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StateKind {
    Backlog,
    Todo,
    Started,
    Done,
    Cancelled,
}

impl StateKind {
    fn from_group(group: &str) -> Self {
        match group {
            "backlog" => Self::Backlog,
            "unstarted" => Self::Todo,
            "started" => Self::Started,
            "completed" => Self::Done,
            "cancelled" => Self::Cancelled,
            _ => Self::Backlog,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Backlog => "Backlog",
            Self::Todo => "Todo",
            Self::Started => "In Progress",
            Self::Done => "Done",
            Self::Cancelled => "Cancelled",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::Started => "started",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Backlog => "◌",
            Self::Todo => "○",
            Self::Started => "◐",
            Self::Done => "●",
            Self::Cancelled => "✕",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Backlog => DIM,
            Self::Todo => PAPER,
            Self::Started => AMBER,
            Self::Done => GREEN,
            Self::Cancelled => RED,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    Urgent,
    High,
    Medium,
    Low,
    None,
}

impl Priority {
    fn from_plane(value: Option<&str>) -> Self {
        match value.unwrap_or("none").to_lowercase().as_str() {
            "urgent" => Self::Urgent,
            "high" => Self::High,
            "medium" => Self::Medium,
            "low" => Self::Low,
            _ => Self::None,
        }
    }

    fn as_plane(self) -> &'static str {
        match self {
            Self::Urgent => "urgent",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::None => "none",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Urgent => "‼",
            Self::High => "↑",
            Self::Medium => "−",
            Self::Low => "↓",
            Self::None => "·",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Urgent => RED,
            Self::High => AMBER,
            Self::Medium => Color::Rgb {
                r: 218,
                g: 201,
                b: 105,
            },
            Self::Low => Color::Rgb {
                r: 119,
                g: 166,
                b: 207,
            },
            Self::None => DIM,
        }
    }
}

#[derive(Debug, Clone)]
struct State {
    id: String,
    name: String,
    kind: StateKind,
}

#[derive(Debug, Clone)]
struct Label {
    id: String,
    name: String,
    color: Color,
}

#[derive(Debug, Clone)]
struct WorkItem {
    id: String,
    key: String,
    sequence_id: i64,
    title: String,
    state_id: String,
    state: StateKind,
    priority: Priority,
    labels: Vec<String>,
    label_ids: Vec<String>,
    due: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    completed_at: Option<String>,
    description: String,
    actions: Vec<String>,
}

#[derive(Debug, Clone)]
struct Project {
    id: String,
    name: String,
    identifier: String,
    states: Vec<State>,
    labels: Vec<Label>,
    items: Vec<WorkItem>,
    loaded_at: Instant,
}

impl Project {
    fn state_by_kind(&self, kind: StateKind) -> Option<&State> {
        self.states.iter().find(|state| state.kind == kind)
    }

    fn state_name(&self, id: &str) -> String {
        self.states
            .iter()
            .find(|state| state.id == id)
            .map(|state| state.name.clone())
            .unwrap_or_else(|| "unknown".to_owned())
    }

    fn total_for(&self, kind: StateKind) -> usize {
        self.items.iter().filter(|item| item.state == kind).count()
    }
}

#[derive(Debug, Clone, Copy)]
struct ListLayout {
    mark: u16,
    priority: u16,
    key: u16,
    title: u16,
    state: u16,
    labels: u16,
    due: u16,
    updated: u16,
}

impl ListLayout {
    fn new(width: u16) -> Self {
        let fixed = LIST_MARK_WIDTH
            + LIST_PRIORITY_WIDTH
            + LIST_KEY_WIDTH
            + LIST_STATE_WIDTH
            + LIST_DUE_WIDTH
            + LIST_UPDATED_WIDTH
            + LIST_GAP * 7;
        let labels = width
            .saturating_sub(fixed + LIST_TITLE_MIN_WIDTH)
            .min(LIST_LABELS_MAX_WIDTH);
        let title = width.saturating_sub(fixed + labels);

        Self {
            mark: LIST_MARK_WIDTH,
            priority: LIST_PRIORITY_WIDTH,
            key: LIST_KEY_WIDTH,
            title,
            state: LIST_STATE_WIDTH,
            labels,
            due: LIST_DUE_WIDTH,
            updated: LIST_UPDATED_WIDTH,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Board,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterMode {
    All,
    Fire,
    Untriaged,
}

impl FilterMode {
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Fire => "urgent+high",
            Self::Untriaged => "untriaged",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    Priority,
    Updated,
    Created,
    Key,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            Self::Priority => "prio",
            Self::Updated => "updated",
            Self::Created => "created",
            Self::Key => "key",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuMode {
    State,
    Priority,
    Label,
    Edit,
    ConfirmWip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Search,
    Command,
    NewLabel,
    BackendModel,
    BackendEffort,
    ProjectName,
    ProjectIdentifier,
    EditTitle,
    EditDue,
}

impl InputMode {
    fn can_redraw_footer_only(self) -> bool {
        !matches!(self, Self::Search)
    }
}

#[derive(Debug, Clone)]
struct ApiLog {
    time: String,
    method: &'static str,
    path: String,
    payload: String,
    status: String,
    ms: u128,
}

#[derive(Debug)]
struct Triage {
    keys: Vec<String>,
    index: usize,
    decided: usize,
    promoted: usize,
    dropped: usize,
}

#[derive(Debug)]
struct PromptView {
    key: String,
    text: String,
    file: String,
    scroll: usize,
}

struct CodexJob {
    key: String,
    backend: AgentBackend,
    comment_path: String,
    pid: u32,
    started: Instant,
    rx: mpsc::Receiver<CodexOutcome>,
}

struct CodexOutcome {
    prompt: Result<String>,
    comment: Option<Result<()>>,
    elapsed_ms: u128,
}

struct DetailView {
    key: String,
    scroll: usize,
    comments: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct BackendWizard {
    selected: AgentBackend,
    claude_model: String,
    claude_effort: String,
}

impl BackendWizard {
    fn from_config(config: &Config) -> Self {
        Self {
            selected: config.agent_backend,
            claude_model: config.claude_model.clone(),
            claude_effort: config.claude_effort.clone(),
        }
    }

    fn cycle(&mut self) {
        self.selected = match self.selected {
            AgentBackend::Codex => AgentBackend::Claude,
            AgentBackend::Claude => AgentBackend::Codex,
        };
    }
}

struct App {
    client: PlaneClient,
    projects: Vec<Project>,
    active_project: usize,
    view: ViewMode,
    column: usize,
    row: usize,
    cursor: usize,
    marks: BTreeSet<String>,
    filter: FilterMode,
    sort: SortMode,
    search: String,
    input_mode: Option<InputMode>,
    input: String,
    input_cursor: usize,
    editing_key: Option<String>,
    new_project_name: Option<String>,
    menu: Option<MenuMode>,
    api_open: bool,
    show_done: bool,
    keys_open: bool,
    notes_open: bool,
    triage: Option<Triage>,
    prompt_view: Option<PromptView>,
    codex_job: Option<CodexJob>,
    detail: Option<DetailView>,
    backend_wizard: Option<BackendWizard>,
    last_idle_draw: Option<Instant>,
    api_log: Vec<ApiLog>,
    status: String,
    busy: Option<String>,
    last_g: Option<Instant>,
    frame: usize,
    should_quit: bool,
    last_size: Option<(u16, u16)>,
    force_clear: bool,
}

impl App {
    fn load(client: PlaneClient) -> Result<Self> {
        let mut api_log = Vec::new();
        let t0 = Instant::now();
        let api_projects = client.projects()?;
        api_log.push(ApiLog::new(
            "GET",
            "/projects/",
            "",
            "200",
            t0.elapsed().as_millis(),
        ));

        let mut projects = Vec::new();
        for api_project in api_projects.into_iter().filter(|project| {
            project.archived_at.is_none()
                && (client.config.wanted_projects.is_empty()
                    || client.config.wanted_projects.iter().any(|wanted| {
                        wanted == &project.name.to_lowercase()
                            || wanted == &project.identifier.to_lowercase()
                    }))
        }) {
            let t0 = Instant::now();
            let api_states = client.states(&api_project.id)?;
            api_log.push(ApiLog::new(
                "GET",
                &format!("/{}/states/", api_project.identifier),
                "",
                "200",
                t0.elapsed().as_millis(),
            ));
            let t0 = Instant::now();
            let api_labels = client.labels(&api_project.id).unwrap_or_default();
            api_log.push(ApiLog::new(
                "GET",
                &format!("/{}/labels/", api_project.identifier),
                "",
                "200",
                t0.elapsed().as_millis(),
            ));
            let t0 = Instant::now();
            let api_items = client.work_items(&api_project.id, client.config.per_page)?;
            api_log.push(ApiLog::new(
                "GET",
                &format!(
                    "/{}/work-items/?per_page={}",
                    api_project.identifier, client.config.per_page
                ),
                "",
                "200",
                t0.elapsed().as_millis(),
            ));

            projects.push(project_from_api(
                api_project,
                api_states,
                api_labels,
                api_items,
            ));
        }
        let wanted = client.config.wanted_projects.clone();
        projects.sort_by_key(|project| {
            wanted
                .iter()
                .position(|wanted| {
                    wanted == &project.name.to_lowercase()
                        || wanted == &project.identifier.to_lowercase()
                })
                .unwrap_or(usize::MAX)
        });

        if projects.is_empty() {
            bail!("no active Plane projects matched --projects");
        }

        Ok(Self {
            client,
            projects,
            active_project: 0,
            view: ViewMode::Board,
            column: 1,
            row: 0,
            cursor: 0,
            marks: BTreeSet::new(),
            filter: FilterMode::All,
            sort: SortMode::Priority,
            search: String::new(),
            input_mode: None,
            input: String::new(),
            input_cursor: 0,
            editing_key: None,
            new_project_name: None,
            menu: None,
            api_open: false,
            show_done: false,
            keys_open: false,
            notes_open: false,
            triage: None,
            prompt_view: None,
            codex_job: None,
            detail: None,
            backend_wizard: None,
            last_idle_draw: None,
            api_log,
            status: "connected · press T to triage · ? for keys".to_owned(),
            busy: None,
            last_g: None,
            frame: 0,
            should_quit: false,
            last_size: None,
            force_clear: true,
        })
    }

    fn run(&mut self) -> Result<()> {
        let _guard = TerminalGuard::enter()?;
        self.draw()?;
        let mut pending_event = None;
        loop {
            if self.should_quit {
                break;
            }
            let next_event = if let Some(event) = pending_event.take() {
                Some(event)
            } else if event::poll(Duration::from_millis(250))? {
                Some(event::read()?)
            } else {
                None
            };
            if let Some(next_event) = next_event {
                match next_event {
                    Event::Key(key) => {
                        let overlay_scroll = self.is_overlay_scroll_key(&key);
                        let input_mode_before = self.input_mode;
                        let force_clear_before = self.force_clear;
                        let coalesce_repeat_keys = self.can_coalesce_repeat_key(&key);
                        self.handle_key(key)?;
                        if self.should_quit {
                            break;
                        }
                        if coalesce_repeat_keys {
                            self.drain_coalesced_repeat_keys(&mut pending_event)?;
                            if self.should_quit {
                                break;
                            }
                        }
                        let codex_redraw = self.poll_codex_job();
                        self.frame = (self.frame + 1) % FRAMES.len();
                        if overlay_scroll && (self.prompt_view.is_some() || self.detail.is_some()) {
                            self.draw_active_overlay()?;
                        } else if self.can_redraw_footer_only_after_key(
                            input_mode_before,
                            force_clear_before,
                            codex_redraw,
                        ) {
                            self.draw_footer_only()?;
                        } else {
                            self.draw()?;
                        }
                    }
                    Event::Resize(_, _) => {
                        self.clamp_selection();
                        self.draw()?;
                    }
                    Event::Mouse(mouse) => {
                        if self.handle_mouse(mouse)? {
                            self.draw_active_overlay()?;
                        }
                    }
                    _ => {}
                }
            } else {
                self.on_tick()?;
            }
        }
        Ok(())
    }

    fn is_overlay_scroll_key(&self, key: &KeyEvent) -> bool {
        (self.prompt_view.is_some() || self.detail.is_some())
            && matches!(
                key.code,
                KeyCode::Char('j')
                    | KeyCode::Down
                    | KeyCode::Char('k')
                    | KeyCode::Up
                    | KeyCode::PageDown
                    | KeyCode::Char('d')
                    | KeyCode::PageUp
                    | KeyCode::Char('u')
                    | KeyCode::Char('g')
                    | KeyCode::Char('G')
            )
    }

    fn can_coalesce_repeat_key(&self, key: &KeyEvent) -> bool {
        if key.modifiers != KeyModifiers::NONE {
            return false;
        }
        if self.prompt_view.is_some() || self.detail.is_some() {
            return self.is_overlay_scroll_key(key);
        }
        if self.keys_open
            || self.notes_open
            || self.triage.is_some()
            || self.backend_wizard.is_some()
            || self.menu.is_some()
            || self.input_mode.is_some()
        {
            return false;
        }
        matches!(
            key.code,
            KeyCode::Char('j')
                | KeyCode::Down
                | KeyCode::Char('k')
                | KeyCode::Up
                | KeyCode::Char('h')
                | KeyCode::Left
                | KeyCode::Char('l')
                | KeyCode::Right
        )
    }

    fn drain_coalesced_repeat_keys(&mut self, pending_event: &mut Option<Event>) -> Result<()> {
        let mut drained = 0;
        while drained < MAX_COALESCED_NAV_KEYS && event::poll(Duration::from_millis(0))? {
            let event = event::read()?;
            match event {
                Event::Key(key) if self.can_coalesce_repeat_key(&key) => {
                    self.handle_key(key)?;
                    drained += 1;
                    if self.should_quit {
                        break;
                    }
                }
                event => {
                    *pending_event = Some(event);
                    break;
                }
            }
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<bool> {
        let delta = match mouse.kind {
            MouseEventKind::ScrollDown => MOUSE_SCROLL_LINES,
            MouseEventKind::ScrollUp => -MOUSE_SCROLL_LINES,
            _ => return Ok(false),
        };
        if self.prompt_view.is_some() {
            self.scroll_prompt_view(delta);
            return Ok(true);
        }
        if self.detail.is_some() {
            self.scroll_detail_view(delta);
            return Ok(true);
        }
        Ok(false)
    }

    fn scroll_prompt_view(&mut self, delta: isize) {
        if let Some(view) = self.prompt_view.as_mut() {
            view.scroll = scroll_offset(view.scroll, delta);
        }
    }

    fn scroll_detail_view(&mut self, delta: isize) {
        if let Some(detail) = self.detail.as_mut() {
            detail.scroll = scroll_offset(detail.scroll, delta);
        }
    }

    fn on_tick(&mut self) -> Result<()> {
        let mut redraw = self.poll_codex_job();
        if let Some(job) = &self.codex_job {
            let elapsed = job.started.elapsed().as_secs();
            self.busy = Some(format!(
                "{} · agent prompt for {} · {elapsed}s · esc cancels",
                job.backend.name(),
                job.key
            ));
            self.frame = (self.frame + 1) % FRAMES.len();
            redraw = true;
        }
        if self.maybe_auto_refresh() {
            redraw = true;
        }
        if !redraw
            && self
                .last_idle_draw
                .is_none_or(|at| at.elapsed() > Duration::from_secs(30))
        {
            redraw = true;
        }
        if redraw {
            self.last_idle_draw = Some(Instant::now());
            self.draw()?;
        }
        Ok(())
    }

    fn poll_codex_job(&mut self) -> bool {
        let Some(job) = &self.codex_job else {
            return false;
        };
        match job.rx.try_recv() {
            Ok(outcome) => {
                let job = self.codex_job.take().expect("codex job present");
                self.busy = None;
                self.finish_codex_job(job, outcome);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                let job = self.codex_job.take().expect("codex job present");
                self.busy = None;
                self.status = format!(
                    "{} worker for {} stopped unexpectedly",
                    job.backend.name(),
                    job.key
                );
                true
            }
        }
    }

    fn finish_codex_job(&mut self, job: CodexJob, outcome: CodexOutcome) {
        let prompt = match outcome.prompt {
            Ok(prompt) => prompt,
            Err(err) => {
                self.api_log.push(ApiLog::new(
                    "AGENT",
                    &job.key,
                    "agent prompt",
                    "err",
                    outcome.elapsed_ms,
                ));
                self.status = format!("{} failed: {err:#}", job.backend.name());
                return;
            }
        };
        self.api_log.push(ApiLog::new(
            "AGENT",
            &job.key,
            "agent prompt",
            "ok",
            outcome.elapsed_ms,
        ));
        let file = save_prompt(&job.key, &prompt).unwrap_or_else(|_| "(not saved)".to_owned());
        let clipboard_note = match copy_to_clipboard(&prompt) {
            Ok(()) => " · copied".to_owned(),
            Err(err) => format!(" · clipboard failed: {err:#}"),
        };
        let comment_note = match outcome.comment {
            Some(Ok(())) => {
                self.api_log.push(ApiLog::new(
                    "POST",
                    &job.comment_path,
                    "agent prompt comment",
                    "201",
                    0,
                ));
                if let Some(index) = self.find_index_by_key(&job.key) {
                    self.project_mut().items[index]
                        .actions
                        .insert(0, "POST comment · agent prompt".to_owned());
                }
                " · commented".to_owned()
            }
            Some(Err(err)) => {
                self.api_log.push(ApiLog::new(
                    "POST",
                    &job.comment_path,
                    "agent prompt comment",
                    "err",
                    0,
                ));
                format!(" · comment failed: {err:#}")
            }
            None => String::new(),
        };
        self.status = format!(
            "agent prompt for {} · saved {file}{clipboard_note}{comment_note}",
            job.key
        );
        self.prompt_view = Some(PromptView {
            key: job.key,
            text: prompt,
            file,
            scroll: 0,
        });
        self.force_clear = true;
    }

    fn cancel_codex_job(&mut self) {
        if let Some(job) = self.codex_job.take() {
            let _ = Command::new("kill")
                .arg(job.pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            self.busy = None;
            self.status = format!("{} cancelled for {}", job.backend.name(), job.key);
        }
    }

    fn maybe_auto_refresh(&mut self) -> bool {
        let mins = self.client.config.auto_refresh_mins;
        if mins == 0
            || self.input_mode.is_some()
            || self.menu.is_some()
            || self.triage.is_some()
            || self.keys_open
            || self.notes_open
            || self.prompt_view.is_some()
            || self.detail.is_some()
            || self.backend_wizard.is_some()
            || self.codex_job.is_some()
            || self.busy.is_some()
        {
            return false;
        }
        if self.project().loaded_at.elapsed() < Duration::from_secs(mins * 60) {
            return false;
        }
        let identifier = self.project().identifier.clone();
        match self.refresh() {
            Ok(()) => self.status = format!("auto-refreshed {identifier}"),
            Err(err) => {
                // touch loaded_at so a dead network does not retry every tick
                self.project_mut().loaded_at = Instant::now();
                self.status = format!("auto-refresh failed: {err:#}");
            }
        }
        true
    }

    fn project(&self) -> &Project {
        &self.projects[self.active_project]
    }

    fn project_mut(&mut self) -> &mut Project {
        &mut self.projects[self.active_project]
    }

    fn filtered_indices_for_state(&self, state: StateKind) -> Vec<usize> {
        let mut indices = self
            .project()
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.state == state && self.matches(item))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        self.sort_indices(&mut indices);
        indices
    }

    fn flat_indices(&self) -> Vec<usize> {
        let mut indices = self
            .board_states()
            .into_iter()
            .flat_map(|state| self.filtered_indices_for_state(state))
            .collect::<Vec<_>>();
        if self.view == ViewMode::List {
            self.sort_indices(&mut indices);
        }
        indices
    }

    fn sort_indices(&self, indices: &mut [usize]) {
        let project = self.project();
        indices.sort_by(|a, b| {
            let left = &project.items[*a];
            let right = &project.items[*b];
            match self.sort {
                SortMode::Priority => left
                    .priority
                    .cmp(&right.priority)
                    .then_with(|| right.updated_at.cmp(&left.updated_at))
                    .then_with(|| right.sequence_id.cmp(&left.sequence_id)),
                SortMode::Updated => right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then_with(|| right.sequence_id.cmp(&left.sequence_id)),
                SortMode::Created => right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| right.sequence_id.cmp(&left.sequence_id)),
                SortMode::Key => right.sequence_id.cmp(&left.sequence_id),
            }
        });
    }

    fn matches(&self, item: &WorkItem) -> bool {
        if !self.search.is_empty() {
            let q = self.search.to_lowercase();
            if !item.title.to_lowercase().contains(&q) && !item.key.to_lowercase().contains(&q) {
                return false;
            }
        }
        match self.filter {
            FilterMode::All => true,
            FilterMode::Fire => matches!(item.priority, Priority::Urgent | Priority::High),
            FilterMode::Untriaged => item.priority == Priority::None,
        }
    }

    fn current_index(&self) -> Option<usize> {
        match self.view {
            ViewMode::Board => {
                let state = self.board_states()[self.column];
                let indices = self.filtered_indices_for_state(state);
                indices
                    .get(self.row.min(indices.len().saturating_sub(1)))
                    .copied()
            }
            ViewMode::List => {
                let indices = self.flat_indices();
                indices
                    .get(self.cursor.min(indices.len().saturating_sub(1)))
                    .copied()
            }
        }
    }

    fn current_item(&self) -> Option<&WorkItem> {
        self.current_index()
            .map(|index| &self.project().items[index])
    }

    fn clamp_selection(&mut self) {
        let states = self.board_states();
        self.column = self.column.min(states.len().saturating_sub(1));
        let state_len = self
            .filtered_indices_for_state(states[self.column])
            .len()
            .saturating_sub(1);
        self.row = self.row.min(state_len);
        let list_len = self.flat_indices().len().saturating_sub(1);
        self.cursor = self.cursor.min(list_len);
    }

    fn target_keys(&self) -> Vec<String> {
        if self.marks.is_empty() {
            self.current_item()
                .map(|item| vec![item.key.clone()])
                .unwrap_or_default()
        } else {
            self.marks.iter().cloned().collect()
        }
    }

    fn find_index_by_key(&self, key: &str) -> Option<usize> {
        self.project().items.iter().position(|item| item.key == key)
    }

    fn wip_limit(&self) -> usize {
        self.client.config.wip_limit
    }

    fn wip_would_exceed(&self, keys: &[String]) -> bool {
        let limit = self.wip_limit();
        if limit == 0 {
            return false;
        }
        let current = self.project().total_for(StateKind::Started);
        let incoming = keys
            .iter()
            .filter(|key| {
                self.find_index_by_key(key)
                    .map(|index| self.project().items[index].state != StateKind::Started)
                    .unwrap_or(false)
            })
            .count();
        current + incoming > limit
    }

    fn run_busy<T, F>(&mut self, message: impl Into<String>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(PlaneClient) -> Result<T> + Send + 'static,
    {
        self.busy = Some(message.into());
        self.frame = (self.frame + 1) % FRAMES.len();
        self.draw()?;

        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f(client));
        });

        loop {
            match rx.try_recv() {
                Ok(result) => {
                    self.busy = None;
                    self.draw()?;
                    return result;
                }
                Err(TryRecvError::Empty) => {
                    self.frame = (self.frame + 1) % FRAMES.len();
                    self.draw()?;
                    if event::poll(Duration::from_millis(90))? {
                        if let Event::Resize(_, _) = event::read()? {
                            self.clamp_selection();
                            self.force_clear = true;
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.busy = None;
                    self.draw()?;
                    bail!("background Plane request stopped unexpectedly");
                }
            }
        }
    }

    fn board_states(&self) -> Vec<StateKind> {
        STATE_ORDER
            .iter()
            .copied()
            .filter(|state| self.show_done || *state != StateKind::Done)
            .collect()
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.keys_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.keys_open = false;
                    self.force_clear = true;
                }
                _ => {}
            }
            return Ok(());
        }
        if self.notes_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('!') | KeyCode::Char('q') => {
                    self.notes_open = false;
                    self.force_clear = true;
                }
                _ => {}
            }
            return Ok(());
        }
        if self.prompt_view.is_some() {
            return self.handle_prompt_view_key(key);
        }
        if self.detail.is_some() {
            return self.handle_detail_key(key);
        }
        if self.triage.is_some() {
            return self.handle_triage_key(key);
        }
        if matches!(
            self.input_mode,
            Some(InputMode::BackendModel | InputMode::BackendEffort)
        ) {
            return self.handle_input_key(self.input_mode.expect("backend input mode"), key);
        }
        if self.backend_wizard.is_some() {
            return self.handle_backend_wizard_key(key);
        }
        if let Some(menu) = self.menu {
            return self.handle_menu_key(menu, key);
        }
        if let Some(mode) = self.input_mode {
            return self.handle_input_key(mode, key);
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if self.codex_job.is_some() {
                    self.cancel_codex_job();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_vertical(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_vertical(-1),
            KeyCode::Char('h') | KeyCode::Left => self.move_column(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_column(1),
            KeyCode::Char('g') => self.handle_g(),
            KeyCode::Char('G') => self.move_end(),
            KeyCode::Char('m') => self.toggle_mark(),
            KeyCode::Char('I') => self.invert_marks(),
            KeyCode::Char('U') => self.marks.clear(),
            KeyCode::Char('v') => {
                self.toggle_view();
                self.force_clear = true;
            }
            KeyCode::Char('D') => {
                self.show_done = !self.show_done;
                self.clamp_selection();
                self.status = if self.show_done {
                    "done column shown".to_owned()
                } else {
                    "done column hidden".to_owned()
                };
                self.force_clear = true;
            }
            KeyCode::Char('s') => self.menu = Some(MenuMode::State),
            KeyCode::Char('p') => self.menu = Some(MenuMode::Priority),
            KeyCode::Char('t') => self.menu = Some(MenuMode::Label),
            KeyCode::Char('e') => self.menu = Some(MenuMode::Edit),
            KeyCode::Char('a') => self.generate_agent_prompt(false)?,
            KeyCode::Char('A') => self.generate_agent_prompt(true)?,
            KeyCode::Enter => self.open_detail()?,
            KeyCode::Char('o') => self.open_targets(),
            KeyCode::Char('n') => {
                self.input_mode = Some(InputMode::Command);
                self.input = "new ".to_owned();
                self.input_cursor = self.input.len();
            }
            KeyCode::Char('T') => self.start_triage(),
            KeyCode::Char('R') => self.refresh()?,
            KeyCode::Char('x') => {
                self.api_open = !self.api_open;
                self.force_clear = true;
            }
            KeyCode::Char('f') => {
                self.cycle_filter();
                self.force_clear = true;
            }
            KeyCode::Char('S') => {
                self.cycle_sort();
                self.force_clear = true;
            }
            KeyCode::Char('/') => {
                self.input_mode = Some(InputMode::Search);
                self.input.clear();
                self.input_cursor = 0;
            }
            KeyCode::Char(':') => {
                self.input_mode = Some(InputMode::Command);
                self.input.clear();
                self.input_cursor = 0;
            }
            KeyCode::Char('?') => self.keys_open = true,
            KeyCode::Char('!') => self.notes_open = true,
            KeyCode::Tab => self.cycle_project(1)?,
            KeyCode::BackTab => self.cycle_project(-1)?,
            KeyCode::Char(ch) if ch.is_ascii_digit() && ch != '0' => {
                let index = ch.to_digit(10).unwrap_or(1) as usize - 1;
                if index < self.projects.len() {
                    self.switch_project(index)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_g(&mut self) {
        let now = Instant::now();
        if self
            .last_g
            .is_some_and(|previous| now.duration_since(previous) < Duration::from_millis(450))
        {
            self.row = 0;
            self.cursor = 0;
        }
        self.last_g = Some(now);
    }

    fn move_vertical(&mut self, delta: isize) {
        match self.view {
            ViewMode::Board => {
                let states = self.board_states();
                let len = self.filtered_indices_for_state(states[self.column]).len();
                self.row = add_clamped(self.row, delta, len.saturating_sub(1));
            }
            ViewMode::List => {
                let len = self.flat_indices().len();
                self.cursor = add_clamped(self.cursor, delta, len.saturating_sub(1));
            }
        }
    }

    fn move_column(&mut self, delta: isize) {
        if self.view != ViewMode::Board {
            return;
        }
        let states = self.board_states();
        self.column = add_clamped(self.column, delta, states.len().saturating_sub(1));
        let len = self.filtered_indices_for_state(states[self.column]).len();
        self.row = self.row.min(len.saturating_sub(1));
    }

    fn move_end(&mut self) {
        match self.view {
            ViewMode::Board => {
                let states = self.board_states();
                let len = self.filtered_indices_for_state(states[self.column]).len();
                self.row = len.saturating_sub(1);
            }
            ViewMode::List => {
                let len = self.flat_indices().len();
                self.cursor = len.saturating_sub(1);
            }
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(key) = self.current_item().map(|item| item.key.clone()) {
            if !self.marks.insert(key.clone()) {
                self.marks.remove(&key);
            }
        }
    }

    fn invert_marks(&mut self) {
        let all = self
            .flat_indices()
            .into_iter()
            .map(|index| self.project().items[index].key.clone())
            .collect::<Vec<_>>();
        self.marks = all
            .into_iter()
            .filter(|key| !self.marks.contains(key))
            .collect();
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            ViewMode::Board => ViewMode::List,
            ViewMode::List => ViewMode::Board,
        };
    }

    fn cycle_filter(&mut self) {
        self.filter = match self.filter {
            FilterMode::All => FilterMode::Fire,
            FilterMode::Fire => FilterMode::Untriaged,
            FilterMode::Untriaged => FilterMode::All,
        };
        self.status = format!("filter → {}", self.filter.label());
    }

    fn cycle_sort(&mut self) {
        self.sort = match self.sort {
            SortMode::Priority => SortMode::Updated,
            SortMode::Updated => SortMode::Created,
            SortMode::Created => SortMode::Key,
            SortMode::Key => SortMode::Priority,
        };
        self.status = format!("sort → {}", self.sort.label());
    }

    fn cycle_project(&mut self, delta: isize) -> Result<()> {
        if self.projects.is_empty() {
            return Ok(());
        }
        let max_index = self.projects.len().saturating_sub(1);
        let next = if delta < 0 {
            self.active_project.checked_sub(1).unwrap_or(max_index)
        } else if self.active_project >= max_index {
            0
        } else {
            self.active_project + 1
        };
        self.switch_project(next)
    }

    fn switch_project(&mut self, index: usize) -> Result<()> {
        self.active_project = index;
        self.column = 1.min(self.board_states().len().saturating_sub(1));
        self.row = 0;
        self.cursor = 0;
        self.marks.clear();
        self.search.clear();
        self.status = format!(
            "project → {} {} · {} loaded items",
            self.project().identifier,
            self.project().name,
            self.project().items.len()
        );
        self.force_clear = true;
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        let project_id = self.project().id.clone();
        let identifier = self.project().identifier.clone();
        let t0 = Instant::now();
        let per_page = self.client.config.per_page;
        let api_items = self.run_busy(format!("GET {identifier} work items"), move |client| {
            client.work_items(&project_id, per_page)
        })?;
        self.api_log.push(ApiLog::new(
            "GET",
            &format!(
                "/{identifier}/work-items/?per_page={}",
                self.client.config.per_page
            ),
            "",
            "200",
            t0.elapsed().as_millis(),
        ));
        let states = self.project().states.clone();
        let labels = self.project().labels.clone();
        let state_lookup = states
            .iter()
            .map(|state| (state.id.clone(), state.kind))
            .collect::<BTreeMap<_, _>>();
        let label_lookup = labels
            .iter()
            .map(|label| (label.id.clone(), label.name.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut items = Vec::new();
        for item in api_items
            .into_iter()
            .filter(|item| item.archived_at.is_none())
        {
            let state_id = item.state_id.or(item.state).unwrap_or_default();
            let mut label_ids = item.label_ids;
            if label_ids.is_empty() {
                label_ids = item.labels.clone();
            }
            let mut label_names = item
                .label_details
                .iter()
                .map(|label| label.name.clone())
                .collect::<Vec<_>>();
            if label_names.is_empty() {
                label_names = label_ids
                    .iter()
                    .filter_map(|id| label_lookup.get(id).cloned())
                    .collect();
            }
            items.push(WorkItem {
                id: item.id,
                key: format!("{identifier}-{}", item.sequence_id),
                sequence_id: item.sequence_id,
                title: item.name,
                state_id: state_id.clone(),
                state: state_lookup
                    .get(&state_id)
                    .copied()
                    .unwrap_or(StateKind::Backlog),
                priority: Priority::from_plane(item.priority.as_deref()),
                labels: label_names,
                label_ids,
                due: item.target_date,
                created_at: parse_dt(item.created_at.as_deref()),
                updated_at: parse_dt(item.updated_at.as_deref()),
                completed_at: item.completed_at,
                description: html_to_text_multiline(item.description_html.as_deref().unwrap_or("")),
                actions: Vec::new(),
            });
        }
        self.project_mut().items = items;
        self.project_mut().loaded_at = Instant::now();
        self.status = format!(
            "refreshed {} · {} loaded items",
            identifier,
            self.project().items.len()
        );
        self.force_clear = true;
        Ok(())
    }

    fn handle_menu_key(&mut self, menu: MenuMode, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::Esc) {
            self.menu = None;
            self.force_clear = true;
            return Ok(());
        }
        match menu {
            MenuMode::State => {
                if let KeyCode::Char(ch) = key.code {
                    if let Some(index) = ch.to_digit(10).and_then(|n| n.checked_sub(1)) {
                        let state = [
                            StateKind::Backlog,
                            StateKind::Todo,
                            StateKind::Started,
                            StateKind::Done,
                            StateKind::Cancelled,
                        ]
                        .get(index as usize)
                        .copied();
                        if let Some(state) = state {
                            if state == StateKind::Started
                                && self.wip_would_exceed(&self.target_keys())
                            {
                                self.menu = Some(MenuMode::ConfirmWip);
                                self.force_clear = true;
                                return Ok(());
                            }
                            self.apply_state(state)?;
                            self.menu = None;
                            self.marks.clear();
                            self.force_clear = true;
                        }
                    }
                }
            }
            MenuMode::ConfirmWip => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.apply_state(StateKind::Started)?;
                    self.menu = None;
                    self.marks.clear();
                    self.force_clear = true;
                }
                KeyCode::Char('n') => {
                    self.menu = None;
                    self.force_clear = true;
                    self.status = "kept out of In Progress — finish something first".to_owned();
                }
                _ => {}
            },
            MenuMode::Priority => {
                if let KeyCode::Char(ch) = key.code {
                    let priority = match ch {
                        'u' => Some(Priority::Urgent),
                        'h' => Some(Priority::High),
                        'm' => Some(Priority::Medium),
                        'l' => Some(Priority::Low),
                        'n' => Some(Priority::None),
                        _ => None,
                    };
                    if let Some(priority) = priority {
                        self.apply_priority(priority)?;
                        self.menu = None;
                        self.marks.clear();
                        self.force_clear = true;
                    }
                }
            }
            MenuMode::Label => {
                if matches!(key.code, KeyCode::Enter) {
                    self.menu = None;
                    self.force_clear = true;
                    return Ok(());
                }
                if let KeyCode::Char(ch) = key.code {
                    if ch == 'n' {
                        self.input_mode = Some(InputMode::NewLabel);
                        self.input.clear();
                        self.input_cursor = 0;
                        self.menu = None;
                        return Ok(());
                    }
                    if let Some(index) = ch.to_digit(10).and_then(|n| n.checked_sub(1)) {
                        self.toggle_label(index as usize)?;
                    }
                }
            }
            MenuMode::Edit => {
                if let KeyCode::Char(ch) = key.code {
                    match ch {
                        't' => self.start_edit_title(),
                        'd' => self.edit_description_in_editor()?,
                        'u' => self.start_edit_due(),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn start_edit_title(&mut self) {
        let Some((key, title)) = self
            .current_item()
            .map(|item| (item.key.clone(), item.title.clone()))
        else {
            self.status = "no item selected".to_owned();
            return;
        };
        self.input = title;
        self.input_cursor = self.input.len();
        self.editing_key = Some(key);
        self.input_mode = Some(InputMode::EditTitle);
        self.menu = None;
    }

    fn edit_description_in_editor(&mut self) -> Result<()> {
        let Some((key, description)) = self
            .current_item()
            .map(|item| (item.key.clone(), item.description.clone()))
        else {
            self.status = "no item selected".to_owned();
            self.menu = None;
            return Ok(());
        };
        self.menu = None;
        self.force_clear = true;
        let edited = match edit_text_in_editor(&key, &description) {
            Ok(Some(edited)) => edited,
            Ok(None) => {
                self.status = format!("description unchanged for {key}");
                return Ok(());
            }
            Err(err) => {
                self.status = format!("editor failed: {err:#}");
                return Ok(());
            }
        };
        let Some(index) = self.find_index_by_key(&key) else {
            self.status = format!("{key} is no longer loaded");
            return Ok(());
        };
        let project_id = self.project().id.clone();
        let item_id = self.project().items[index].id.clone();
        let path = format!("/{}/work-items/{key}/", self.project().identifier);
        let description_html = text_to_description_html(&edited);
        let t0 = Instant::now();
        let body = json!({ "description_html": description_html });
        self.run_busy(format!("PATCH {key} description"), move |client| {
            client.update_work_item(&project_id, &item_id, body)
        })?;
        self.api_log.push(ApiLog::new(
            "PATCH",
            &path,
            "description",
            "200",
            t0.elapsed().as_millis(),
        ));
        let item = &mut self.project_mut().items[index];
        item.description = edited;
        item.updated_at = Some(Utc::now());
        item.actions.insert(0, "PATCH description".to_owned());
        self.status = format!("edited description for {key} in $EDITOR");
        Ok(())
    }

    fn start_edit_due(&mut self) {
        let Some((key, due)) = self
            .current_item()
            .map(|item| (item.key.clone(), item.due.clone().unwrap_or_default()))
        else {
            self.status = "no item selected".to_owned();
            return;
        };
        self.input = due;
        self.input_cursor = self.input.len();
        self.editing_key = Some(key);
        self.input_mode = Some(InputMode::EditDue);
        self.menu = None;
    }

    fn handle_input_key(&mut self, mode: InputMode, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = None;
                self.input.clear();
                self.input_cursor = 0;
                self.editing_key = None;
                self.new_project_name = None;
                if mode == InputMode::Search {
                    self.search.clear();
                    self.force_clear = true;
                }
            }
            KeyCode::Enter => {
                let keep_input = match mode {
                    InputMode::Search => {
                        self.search = self.input.clone();
                        self.status = format!("search → /{}", self.search);
                        self.force_clear = true;
                        false
                    }
                    InputMode::Command => self.run_command()?,
                    InputMode::NewLabel => {
                        self.create_label_from_input()?;
                        false
                    }
                    InputMode::BackendModel => self.update_backend_wizard_model(),
                    InputMode::BackendEffort => self.update_backend_wizard_effort(),
                    InputMode::ProjectName => {
                        self.advance_project_wizard_name();
                        true
                    }
                    InputMode::ProjectIdentifier => self.create_project_from_wizard()?,
                    InputMode::EditTitle => {
                        self.apply_title_edit()?;
                        false
                    }
                    InputMode::EditDue => {
                        self.apply_due_edit()?;
                        false
                    }
                };
                if !keep_input {
                    self.clear_input_state();
                }
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    let previous = previous_char_boundary(&self.input, self.input_cursor);
                    self.input.replace_range(previous..self.input_cursor, "");
                    self.input_cursor = previous;
                }
                if mode == InputMode::Search {
                    self.search = self.input.clone();
                    self.force_clear = true;
                }
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input.len() {
                    let next = next_char_boundary(&self.input, self.input_cursor);
                    self.input.replace_range(self.input_cursor..next, "");
                }
                if mode == InputMode::Search {
                    self.search = self.input.clone();
                    self.force_clear = true;
                }
            }
            KeyCode::Left => {
                self.input_cursor = previous_char_boundary(&self.input, self.input_cursor)
            }
            KeyCode::Right => {
                self.input_cursor = next_char_boundary(&self.input, self.input_cursor)
            }
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = self.input.len(),
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.insert(self.input_cursor, ch);
                self.input_cursor += ch.len_utf8();
                if mode == InputMode::Search {
                    self.search = self.input.clone();
                    self.force_clear = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn clear_input_state(&mut self) {
        self.input_mode = None;
        self.input.clear();
        self.input_cursor = 0;
        self.editing_key = None;
        self.new_project_name = None;
    }

    fn run_command(&mut self) -> Result<bool> {
        let input = self.input.clone();
        let mut parts = input.trim().splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        let keep_input = match command {
            "new" => {
                self.create_item(rest)?;
                false
            }
            "project" if rest.is_empty() || rest == "new" => {
                self.start_project_wizard();
                true
            }
            "agent" | "prompt" if rest == "post" => {
                self.generate_agent_prompt(true)?;
                false
            }
            "agent" | "prompt" => {
                self.generate_agent_prompt(false)?;
                false
            }
            "backend" => {
                if rest.is_empty() {
                    self.start_backend_wizard();
                } else {
                    self.configure_backend(rest);
                }
                false
            }
            "triage" => {
                self.start_triage();
                false
            }
            "state" => {
                self.menu = Some(MenuMode::State);
                false
            }
            "priority" => {
                self.menu = Some(MenuMode::Priority);
                false
            }
            "label" => {
                self.menu = Some(MenuMode::Label);
                false
            }
            "open" => {
                self.open_targets();
                false
            }
            "view" => {
                self.toggle_view();
                self.force_clear = true;
                false
            }
            "refresh" => {
                self.refresh()?;
                false
            }
            "api" => {
                self.api_open = !self.api_open;
                self.force_clear = true;
                false
            }
            "help" => {
                self.keys_open = true;
                false
            }
            "filter" if rest == "fire" => {
                self.filter = FilterMode::Fire;
                self.force_clear = true;
                false
            }
            "filter" if rest == "untriaged" => {
                self.filter = FilterMode::Untriaged;
                self.force_clear = true;
                false
            }
            "filter" if rest == "clear" => {
                self.filter = FilterMode::All;
                self.force_clear = true;
                false
            }
            "sort" => {
                self.cycle_sort();
                self.force_clear = true;
                false
            }
            "" => false,
            other => {
                self.status = format!("unknown command :{other}");
                false
            }
        };
        Ok(keep_input)
    }

    fn start_project_wizard(&mut self) {
        self.input_mode = Some(InputMode::ProjectName);
        self.input.clear();
        self.input_cursor = 0;
        self.editing_key = None;
        self.new_project_name = None;
        self.status = "new project: enter name".to_owned();
    }

    fn advance_project_wizard_name(&mut self) {
        let name = self.input.trim().to_owned();
        if name.is_empty() {
            self.status = "project name is required".to_owned();
            self.input_mode = Some(InputMode::ProjectName);
            return;
        }
        if self
            .projects
            .iter()
            .any(|project| project.name.eq_ignore_ascii_case(&name))
        {
            self.status = format!("project already exists: {name}");
            self.input_mode = Some(InputMode::ProjectName);
            return;
        }
        let identifier = project_identifier_from_name(&name);
        self.new_project_name = Some(name);
        self.input = identifier;
        self.input_cursor = self.input.len();
        self.input_mode = Some(InputMode::ProjectIdentifier);
        self.status = "new project: confirm identifier".to_owned();
    }

    fn create_project_from_wizard(&mut self) -> Result<bool> {
        let Some(name) = self.new_project_name.clone() else {
            self.status = "project wizard lost the name; start :project again".to_owned();
            return Ok(false);
        };
        let identifier = normalize_project_identifier(&self.input);
        if identifier.is_empty() {
            self.status = "project identifier is required".to_owned();
            self.input_mode = Some(InputMode::ProjectIdentifier);
            return Ok(true);
        }
        if self.projects.iter().any(|project| {
            project.identifier.eq_ignore_ascii_case(&identifier)
                || project.name.eq_ignore_ascii_case(&name)
        }) {
            self.status = format!("project already exists: {identifier} {name}");
            self.input = identifier;
            self.input_cursor = self.input.len();
            self.input_mode = Some(InputMode::ProjectIdentifier);
            return Ok(true);
        }

        let t0 = Instant::now();
        let body = json!({
            "name": name,
            "identifier": identifier,
        });
        let api_project = self.run_busy(format!("POST project {identifier}"), move |client| {
            client.create_project(body)
        })?;
        self.api_log.push(ApiLog::new(
            "POST",
            "/projects/",
            &format!("{} {}", api_project.identifier, api_project.name),
            "201",
            t0.elapsed().as_millis(),
        ));

        let remember_note =
            match remember_project(&self.client.config.workspace, &api_project.identifier) {
                Ok(()) => String::new(),
                Err(err) => format!(" · remember failed: {err:#}"),
            };
        let loaded = self.load_created_project(api_project)?;
        let identifier = loaded.identifier.clone();
        let name = loaded.name.clone();
        self.projects.push(loaded);
        let index = self.projects.len().saturating_sub(1);
        self.client
            .config
            .wanted_projects
            .push(identifier.to_lowercase());
        self.switch_project(index)?;
        self.status = format!("created project {identifier} {name}{remember_note}");
        Ok(false)
    }

    fn load_created_project(&mut self, api_project: ApiProject) -> Result<Project> {
        let project_id = api_project.id.clone();
        let identifier = api_project.identifier.clone();
        let per_page = self.client.config.per_page;
        let t0 = Instant::now();
        let project = self.run_busy(format!("GET {identifier} project data"), move |client| {
            let states = client.states(&project_id)?;
            let labels = client.labels(&project_id).unwrap_or_default();
            let items = client.work_items(&project_id, per_page)?;
            Ok(project_from_api(api_project, states, labels, items))
        })?;
        self.api_log.push(ApiLog::new(
            "GET",
            &format!("/{identifier}/bootstrap/"),
            "states labels items",
            "200",
            t0.elapsed().as_millis(),
        ));
        Ok(project)
    }

    fn apply_state(&mut self, state: StateKind) -> Result<()> {
        let state_id = self
            .project()
            .state_by_kind(state)
            .map(|state| state.id.clone())
            .ok_or_else(|| anyhow!("project has no {} state", state.name()))?;
        let keys = self.target_keys();
        for key in keys {
            let Some(index) = self.find_index_by_key(&key) else {
                continue;
            };
            let project_id = self.project().id.clone();
            let item_id = self.project().items[index].id.clone();
            let path = format!("/{}/work-items/{key}/", self.project().identifier);
            let t0 = Instant::now();
            let body = json!({ "state": state_id.clone() });
            let raw = self.run_busy(format!("PATCH {key} state"), move |client| {
                client.update_work_item(&project_id, &item_id, body)
            })?;
            self.api_log.push(ApiLog::new(
                "PATCH",
                &path,
                &format!("state={}", state.slug()),
                "200",
                t0.elapsed().as_millis(),
            ));
            let persisted_state_id = raw
                .get("state")
                .or_else(|| raw.get("state_id"))
                .and_then(Value::as_str)
                .unwrap_or(&state_id)
                .to_owned();
            let persisted_state = self
                .project()
                .states
                .iter()
                .find(|state| state.id == persisted_state_id)
                .map(|state| state.kind)
                .unwrap_or(state);
            let item = &mut self.project_mut().items[index];
            item.state = persisted_state;
            item.state_id = persisted_state_id;
            item.updated_at = Some(Utc::now());
            item.actions
                .insert(0, format!("PATCH state → {}", persisted_state.name()));
        }
        self.status = format!("state → {} for target item(s)", state.name());
        Ok(())
    }

    fn apply_priority(&mut self, priority: Priority) -> Result<()> {
        let keys = self.target_keys();
        for key in keys {
            let Some(index) = self.find_index_by_key(&key) else {
                continue;
            };
            let project_id = self.project().id.clone();
            let item_id = self.project().items[index].id.clone();
            let path = format!("/{}/work-items/{key}/", self.project().identifier);
            let t0 = Instant::now();
            let body = json!({ "priority": priority.as_plane() });
            self.run_busy(format!("PATCH {key} priority"), move |client| {
                client.update_work_item(&project_id, &item_id, body)
            })?;
            self.api_log.push(ApiLog::new(
                "PATCH",
                &path,
                &format!("priority={}", priority.as_plane()),
                "200",
                t0.elapsed().as_millis(),
            ));
            let item = &mut self.project_mut().items[index];
            item.priority = priority;
            item.updated_at = Some(Utc::now());
            item.actions
                .insert(0, format!("PATCH priority → {}", priority.as_plane()));
        }
        self.status = format!("priority → {} for target item(s)", priority.as_plane());
        Ok(())
    }

    fn apply_title_edit(&mut self) -> Result<()> {
        let title = self.input.trim().to_owned();
        if title.is_empty() {
            self.status = "title cannot be empty".to_owned();
            return Ok(());
        }
        let Some(key) = self.editing_key.clone() else {
            self.status = "no edit target".to_owned();
            return Ok(());
        };
        let Some(index) = self.find_index_by_key(&key) else {
            self.status = format!("{key} is no longer loaded");
            return Ok(());
        };
        let project_id = self.project().id.clone();
        let item_id = self.project().items[index].id.clone();
        let path = format!("/{}/work-items/{key}/", self.project().identifier);
        let t0 = Instant::now();
        let body = json!({ "name": title.clone() });
        self.run_busy(format!("PATCH {key} title"), move |client| {
            client.update_work_item(&project_id, &item_id, body)
        })?;
        self.api_log.push(ApiLog::new(
            "PATCH",
            &path,
            "name",
            "200",
            t0.elapsed().as_millis(),
        ));
        let item = &mut self.project_mut().items[index];
        item.title = title;
        item.updated_at = Some(Utc::now());
        item.actions.insert(0, "PATCH title".to_owned());
        self.status = format!("edited title for {key}");
        Ok(())
    }

    fn apply_due_edit(&mut self) -> Result<()> {
        let due = self.input.trim().to_owned();
        if !due.is_empty() && !looks_like_date(&due) {
            self.status = "due date must be YYYY-MM-DD, or blank to clear".to_owned();
            return Ok(());
        }
        let Some(key) = self.editing_key.clone() else {
            self.status = "no edit target".to_owned();
            return Ok(());
        };
        let Some(index) = self.find_index_by_key(&key) else {
            self.status = format!("{key} is no longer loaded");
            return Ok(());
        };
        let project_id = self.project().id.clone();
        let item_id = self.project().items[index].id.clone();
        let path = format!("/{}/work-items/{key}/", self.project().identifier);
        let target_date = if due.is_empty() {
            Value::Null
        } else {
            Value::String(due.clone())
        };
        let t0 = Instant::now();
        let body = json!({ "target_date": target_date });
        self.run_busy(format!("PATCH {key} due"), move |client| {
            client.update_work_item(&project_id, &item_id, body)
        })?;
        self.api_log.push(ApiLog::new(
            "PATCH",
            &path,
            "due",
            "200",
            t0.elapsed().as_millis(),
        ));
        let item = &mut self.project_mut().items[index];
        item.due = if due.is_empty() { None } else { Some(due) };
        item.updated_at = Some(Utc::now());
        item.actions.insert(0, "PATCH due date".to_owned());
        self.status = format!("edited due date for {key}");
        Ok(())
    }

    fn toggle_label(&mut self, label_index: usize) -> Result<()> {
        let Some(label) = self.project().labels.get(label_index).cloned() else {
            return Ok(());
        };
        let keys = self.target_keys();
        for key in keys {
            let Some(index) = self.find_index_by_key(&key) else {
                continue;
            };
            let project_id = self.project().id.clone();
            let item_id = self.project().items[index].id.clone();
            let mut label_ids = self.project().items[index].label_ids.clone();
            let adding = if label_ids.contains(&label.id) {
                label_ids.retain(|id| id != &label.id);
                false
            } else {
                label_ids.push(label.id.clone());
                true
            };
            let path = format!("/{}/work-items/{key}/", self.project().identifier);
            let t0 = Instant::now();
            let requested_label_ids = label_ids.clone();
            let body = json!({ "labels": requested_label_ids });
            let raw = self.run_busy(format!("PATCH {key} labels"), move |client| {
                client.update_work_item(&project_id, &item_id, body)
            })?;
            self.api_log.push(ApiLog::new(
                "PATCH",
                &path,
                &format!("label {}{}", if adding { "+" } else { "-" }, label.name),
                "200",
                t0.elapsed().as_millis(),
            ));
            let persisted_label_ids = string_array_field(&raw, "labels")
                .or_else(|| string_array_field(&raw, "label_ids"))
                .unwrap_or(label_ids);
            let persisted_label_names = persisted_label_ids
                .iter()
                .filter_map(|id| {
                    self.project()
                        .labels
                        .iter()
                        .find(|label| &label.id == id)
                        .map(|label| label.name.clone())
                })
                .collect::<Vec<_>>();
            let item = &mut self.project_mut().items[index];
            item.label_ids = persisted_label_ids;
            item.labels = persisted_label_names;
            item.updated_at = Some(Utc::now());
            item.actions.insert(
                0,
                format!(
                    "PATCH label {}{}",
                    if adding { "+" } else { "-" },
                    label.name
                ),
            );
        }
        self.status = format!("toggled label {}", label.name);
        Ok(())
    }

    fn create_label_from_input(&mut self) -> Result<()> {
        let name = self.input.trim().to_owned();
        if name.is_empty() {
            self.status = "new label needs a name".to_owned();
            self.menu = Some(MenuMode::Label);
            return Ok(());
        }
        if self
            .project()
            .labels
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(&name))
        {
            self.status = format!("label already exists: {name}");
            self.menu = Some(MenuMode::Label);
            return Ok(());
        }

        let project_id = self.project().id.clone();
        let path = format!("/{}/labels/", self.project().identifier);
        let color = default_label_color(self.project().labels.len());
        let t0 = Instant::now();
        let body = json!({
            "name": name,
            "color": color,
        });
        let api_label = self.run_busy(format!("POST label {name}"), move |client| {
            client.create_label(&project_id, body)
        })?;
        self.api_log.push(ApiLog::new(
            "POST",
            &path,
            "label",
            "201",
            t0.elapsed().as_millis(),
        ));
        let label_name = api_label.name.clone();
        self.project_mut().labels.push(Label {
            id: api_label.id,
            name: api_label.name,
            color: parse_hex_color(api_label.color.as_deref().unwrap_or(color)),
        });
        self.status = format!("created label {label_name}");
        self.menu = Some(MenuMode::Label);
        Ok(())
    }

    fn create_item(&mut self, input: &str) -> Result<()> {
        let (title, priority, label_ids, unknown_labels) =
            parse_new_item_tokens(input, &self.project().labels);
        if title.is_empty() {
            self.status = ":new needs a title".to_owned();
            return Ok(());
        }
        let project_id = self.project().id.clone();
        let state = self.default_new_item_state();
        let state_id = self
            .project()
            .state_by_kind(state)
            .map(|state| state.id.clone());
        let mut body = json!({ "name": title, "priority": priority.as_plane() });
        if let Some(state_id) = state_id {
            body["state"] = Value::String(state_id);
        }
        if !label_ids.is_empty() {
            body["labels"] = json!(label_ids);
        }
        let label_count = label_ids.len();
        let t0 = Instant::now();
        let raw = self.run_busy(format!("POST item in {}", state.name()), move |client| {
            client.create_work_item(&project_id, body)
        })?;
        self.api_log.push(ApiLog::new(
            "POST",
            &format!("/{}/work-items/", self.project().identifier),
            &title,
            "201",
            t0.elapsed().as_millis(),
        ));
        let item: ApiItem = serde_json::from_value(raw)?;
        self.refresh()?;
        let mut notes = String::new();
        if priority != Priority::None {
            notes.push_str(&format!(" · {}", priority.as_plane()));
        }
        if label_count > 0 {
            notes.push_str(&format!(" · {label_count} label(s)"));
        }
        if !unknown_labels.is_empty() {
            notes.push_str(&format!(" · unknown labels: {}", unknown_labels.join(", ")));
        }
        self.status = format!(
            "created {}-{} in {}{notes}",
            self.project().identifier,
            item.sequence_id,
            state.name()
        );
        Ok(())
    }

    fn default_new_item_state(&self) -> StateKind {
        match self.view {
            ViewMode::Board => self.board_states()[self.column],
            ViewMode::List => self
                .current_item()
                .map(|item| item.state)
                .unwrap_or(StateKind::Backlog),
        }
    }

    fn open_targets(&mut self) {
        let targets = self.target_keys();
        for key in &targets {
            let url = format!(
                "{}/{}/browse/{key}",
                self.client.config.base_url, self.client.config.workspace
            );
            let _ = Command::new("open").arg(&url).status();
            self.api_log.push(ApiLog::new("OPEN", &url, "", "ok", 0));
        }
        self.status = format!("opened {} item(s)", targets.len());
    }

    fn start_triage(&mut self) {
        let keys = self
            .project()
            .items
            .iter()
            .filter(|item| item.state == StateKind::Backlog && item.priority == Priority::None)
            .map(|item| item.key.clone())
            .collect::<Vec<_>>();
        if keys.is_empty() {
            self.status = "nothing untriaged in loaded backlog".to_owned();
            return;
        }
        self.triage = Some(Triage {
            keys,
            index: 0,
            decided: 0,
            promoted: 0,
            dropped: 0,
        });
    }

    fn handle_triage_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            let decided = self
                .triage
                .as_ref()
                .map(|triage| triage.decided)
                .unwrap_or(0);
            self.triage = None;
            self.status = format!("triage ended · {decided} decisions");
            self.force_clear = true;
            return Ok(());
        }

        let Some(triage) = self.triage.as_ref() else {
            return Ok(());
        };
        if triage.index >= triage.keys.len() {
            self.triage = None;
            self.status = "triage page complete".to_owned();
            self.force_clear = true;
            return Ok(());
        }
        let current_key = triage.keys[triage.index].clone();
        let mut advance = false;
        let mut promoted = false;
        let mut dropped = false;

        match key.code {
            KeyCode::Enter => advance = true,
            KeyCode::Char('u') => {
                self.with_single_target(&current_key, |app| app.apply_priority(Priority::Urgent))?;
                advance = true;
            }
            KeyCode::Char('h') => {
                self.with_single_target(&current_key, |app| app.apply_priority(Priority::High))?;
                advance = true;
            }
            KeyCode::Char('m') => {
                self.with_single_target(&current_key, |app| app.apply_priority(Priority::Medium))?;
                advance = true;
            }
            KeyCode::Char('l') => {
                self.with_single_target(&current_key, |app| app.apply_priority(Priority::Low))?;
                advance = true;
            }
            KeyCode::Char('n') => advance = true,
            KeyCode::Char('2') => {
                self.with_single_target(&current_key, |app| app.apply_state(StateKind::Todo))?;
                advance = true;
                promoted = true;
            }
            KeyCode::Char('3') => {
                if self.wip_would_exceed(std::slice::from_ref(&current_key)) {
                    self.status = format!(
                        "In Progress is full ({}/{}) — finish something first",
                        self.project().total_for(StateKind::Started),
                        self.wip_limit()
                    );
                } else {
                    self.with_single_target(&current_key, |app| {
                        app.apply_state(StateKind::Started)
                    })?;
                    advance = true;
                    promoted = true;
                }
            }
            KeyCode::Char('5') => {
                self.with_single_target(&current_key, |app| app.apply_state(StateKind::Cancelled))?;
                advance = true;
                dropped = true;
            }
            _ => {}
        }

        if advance {
            if let Some(triage) = self.triage.as_mut() {
                triage.index += 1;
                triage.decided += 1;
                if promoted {
                    triage.promoted += 1;
                }
                if dropped {
                    triage.dropped += 1;
                }
            }
        }
        Ok(())
    }

    fn with_single_target<F>(&mut self, key: &str, f: F) -> Result<()>
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        let old_marks = std::mem::take(&mut self.marks);
        self.marks.insert(key.to_owned());
        let result = f(self);
        self.marks = old_marks;
        result
    }

    fn handle_prompt_view_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.prompt_view = None;
                self.force_clear = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_prompt_view(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_prompt_view(-1);
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                self.scroll_prompt_view(10);
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                self.scroll_prompt_view(-10);
            }
            KeyCode::Char('g') => {
                if let Some(view) = self.prompt_view.as_mut() {
                    view.scroll = 0;
                }
            }
            KeyCode::Char('G') => {
                if let Some(view) = self.prompt_view.as_mut() {
                    view.scroll = usize::MAX / 2;
                }
            }
            KeyCode::Char('y') => {
                if let Some((text, item_key)) = self
                    .prompt_view
                    .as_ref()
                    .map(|view| (view.text.clone(), view.key.clone()))
                {
                    self.status = match copy_to_clipboard(&text) {
                        Ok(()) => format!("copied agent prompt for {item_key}"),
                        Err(err) => format!("clipboard failed: {err:#}"),
                    };
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn open_detail(&mut self) -> Result<()> {
        let Some(item) = self.current_item() else {
            self.status = "no item selected".to_owned();
            return Ok(());
        };
        let key = item.key.clone();
        let item_id = item.id.clone();
        let project_id = self.project().id.clone();
        let path = format!("/{}/work-items/{key}/comments/", self.project().identifier);
        let t0 = Instant::now();
        let result = self.run_busy(format!("GET comments for {key}"), move |client| {
            client.list_comments(&project_id, &item_id)
        });
        let comments = match result {
            Ok(mut list) => {
                self.api_log.push(ApiLog::new(
                    "GET",
                    &path,
                    "",
                    "200",
                    t0.elapsed().as_millis(),
                ));
                list.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                list.into_iter()
                    .map(|comment| {
                        let when = comment
                            .created_at
                            .as_deref()
                            .map(|value| {
                                value.chars().take(16).collect::<String>().replace('T', " ")
                            })
                            .unwrap_or_else(|| "unknown".to_owned());
                        let text =
                            html_to_text_multiline(comment.comment_html.as_deref().unwrap_or(""));
                        (when, text)
                    })
                    .collect()
            }
            Err(err) => {
                self.api_log.push(ApiLog::new(
                    "GET",
                    &path,
                    "",
                    "err",
                    t0.elapsed().as_millis(),
                ));
                self.status = format!("comments fetch failed: {err:#}");
                Vec::new()
            }
        };
        self.detail = Some(DetailView {
            key,
            scroll: 0,
            comments,
        });
        self.force_clear = true;
        Ok(())
    }

    fn handle_detail_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                self.detail = None;
                self.force_clear = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_detail_view(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_detail_view(-1);
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                self.scroll_detail_view(10);
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                self.scroll_detail_view(-10);
            }
            KeyCode::Char('g') => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = 0;
                }
            }
            KeyCode::Char('G') => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = usize::MAX / 2;
                }
            }
            KeyCode::Char('o') => self.open_targets(),
            KeyCode::Char('a') => self.generate_agent_prompt(false)?,
            KeyCode::Char('A') => self.generate_agent_prompt(true)?,
            _ => {}
        }
        Ok(())
    }

    fn start_backend_wizard(&mut self) {
        self.backend_wizard = Some(BackendWizard::from_config(&self.client.config));
        self.menu = None;
        self.status =
            "select agent backend · arrows/1/2 choose · m model · e effort · enter save".to_owned();
        self.force_clear = true;
    }

    fn handle_backend_wizard_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.backend_wizard = None;
                self.status = format!("backend unchanged: {}", self.client.config.agent_summary());
                self.force_clear = true;
            }
            KeyCode::Enter | KeyCode::Char('y') => self.apply_backend_wizard(),
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::BackTab => {
                if let Some(wizard) = self.backend_wizard.as_mut() {
                    wizard.cycle();
                    self.status = format!("backend choice → {}", wizard.selected.name());
                    self.force_clear = true;
                }
            }
            KeyCode::Char('1') => self.select_backend_wizard_choice(AgentBackend::Codex),
            KeyCode::Char('2') => self.select_backend_wizard_choice(AgentBackend::Claude),
            KeyCode::Char('m') => self.start_backend_wizard_model_edit(),
            KeyCode::Char('e') => self.start_backend_wizard_effort_edit(),
            _ => {}
        }
        Ok(())
    }

    fn select_backend_wizard_choice(&mut self, backend: AgentBackend) {
        if let Some(wizard) = self.backend_wizard.as_mut() {
            wizard.selected = backend;
            self.status = format!("backend choice → {}", backend.name());
            self.force_clear = true;
        }
    }

    fn start_backend_wizard_model_edit(&mut self) {
        let Some(wizard) = self.backend_wizard.as_mut() else {
            return;
        };
        wizard.selected = AgentBackend::Claude;
        self.input = wizard.claude_model.clone();
        self.input_cursor = self.input.len();
        self.input_mode = Some(InputMode::BackendModel);
        self.status = "editing Claude model · enter accepts · esc keeps previous".to_owned();
        self.force_clear = true;
    }

    fn start_backend_wizard_effort_edit(&mut self) {
        let Some(wizard) = self.backend_wizard.as_mut() else {
            return;
        };
        wizard.selected = AgentBackend::Claude;
        self.input = wizard.claude_effort.clone();
        self.input_cursor = self.input.len();
        self.input_mode = Some(InputMode::BackendEffort);
        self.status = "editing Claude effort · low/medium/high/xhigh/max".to_owned();
        self.force_clear = true;
    }

    fn update_backend_wizard_model(&mut self) -> bool {
        let value = self.input.trim();
        let Some(wizard) = self.backend_wizard.as_mut() else {
            return false;
        };
        if value.is_empty() {
            self.status = "Claude model cannot be empty".to_owned();
            return true;
        }
        wizard.claude_model = value.to_owned();
        wizard.selected = AgentBackend::Claude;
        self.status = format!("Claude model → {}", wizard.claude_model);
        self.force_clear = true;
        false
    }

    fn update_backend_wizard_effort(&mut self) -> bool {
        let value = self.input.trim();
        let Some(wizard) = self.backend_wizard.as_mut() else {
            return false;
        };
        if value.is_empty() {
            self.status = "Claude effort cannot be empty".to_owned();
            return true;
        }
        wizard.claude_effort = value.to_owned();
        wizard.selected = AgentBackend::Claude;
        self.status = format!("Claude effort → {}", wizard.claude_effort);
        self.force_clear = true;
        false
    }

    fn apply_backend_wizard(&mut self) {
        let Some(wizard) = self.backend_wizard.take() else {
            return;
        };
        self.set_agent_backend(
            wizard.selected,
            Some(wizard.claude_model),
            Some(wizard.claude_effort),
        );
        self.force_clear = true;
    }

    fn set_agent_backend(
        &mut self,
        backend: AgentBackend,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let (model, effort, summary) = {
            let config = &mut self.client.config;
            config.agent_backend = backend;
            if let Some(model) = model {
                config.claude_model = model;
            }
            if let Some(effort) = effort {
                config.claude_effort = effort;
            }
            (
                config.claude_model.clone(),
                config.claude_effort.clone(),
                config.agent_summary(),
            )
        };
        if let Err(err) = save_agent_prefs(backend, &model, &effort) {
            self.status = format!("agent backend: {summary} (not saved: {err:#})");
            return;
        }
        self.status = format!("agent backend: {summary}");
    }

    fn configure_backend(&mut self, rest: &str) {
        if rest.is_empty() {
            self.start_backend_wizard();
            return;
        }
        let mut parts = rest.split_whitespace();
        if let Some(first) = parts.next() {
            let Some(backend) = AgentBackend::parse(first) else {
                self.status = "usage: :backend codex | :backend claude [model] [effort]".to_owned();
                return;
            };
            let model = (backend == AgentBackend::Claude)
                .then(|| parts.next().map(str::to_owned))
                .flatten();
            let effort = (backend == AgentBackend::Claude)
                .then(|| parts.next().map(str::to_owned))
                .flatten();
            self.set_agent_backend(backend, model, effort);
        }
    }

    fn generate_agent_prompt(&mut self, post_comment: bool) -> Result<()> {
        if let Some(job) = &self.codex_job {
            self.status = format!(
                "{} already generating for {} · esc cancels",
                job.backend.name(),
                job.key
            );
            return Ok(());
        }
        let Some(item) = self.current_item() else {
            self.status = "no item selected".to_owned();
            return Ok(());
        };
        let item_key = item.key.clone();
        let item_id = item.id.clone();
        let meta_prompt = self.build_meta_prompt(item);
        let project_id = self.project().id.clone();
        let comment_path = format!(
            "/{}/work-items/{item_key}/comments/",
            self.project().identifier
        );
        let config = &self.client.config;
        let out_file = std::env::temp_dir().join(format!(
            "plane-tui-agent-prompt-{}-{item_key}.md",
            std::process::id()
        ));
        let backend = config.agent_backend;
        let child = match spawn_agent(config, &out_file) {
            Ok(child) => child,
            Err(err) => {
                self.status = format!("{} failed: {err:#}", backend.name());
                return Ok(());
            }
        };
        let pid = child.id();
        let agent_bin = config.agent_bin().to_owned();
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let t0 = Instant::now();
            let prompt = complete_agent(child, backend, &agent_bin, &out_file, &meta_prompt);
            let comment = match (&prompt, post_comment) {
                (Ok(prompt), true) => {
                    let body =
                        json!({ "comment_html": format!("<pre>{}</pre>", escape_html(prompt)) });
                    Some(
                        client
                            .create_comment(&project_id, &item_id, body)
                            .map(|_| ()),
                    )
                }
                _ => None,
            };
            let _ = tx.send(CodexOutcome {
                prompt,
                comment,
                elapsed_ms: t0.elapsed().as_millis(),
            });
        });
        self.codex_job = Some(CodexJob {
            key: item_key.clone(),
            backend,
            comment_path,
            pid,
            started: Instant::now(),
            rx,
        });
        self.busy = Some(format!(
            "{} · agent prompt for {item_key} · esc cancels",
            backend.name()
        ));
        self.status = format!(
            "{} started for {item_key}{} · keep working, it runs in the background",
            backend.name(),
            if post_comment {
                " · will post comment"
            } else {
                ""
            }
        );
        Ok(())
    }

    fn build_meta_prompt(&self, item: &WorkItem) -> String {
        let project = self.project();
        let config = &self.client.config;
        let context = config
            .context_file
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
            .unwrap_or_else(|| BUSINESS_CONTEXT.to_owned());
        let labels = if item.labels.is_empty() {
            "none".to_owned()
        } else {
            item.labels.join(", ")
        };
        let description = if item.description.trim().is_empty() {
            "(no description)".to_owned()
        } else {
            item.description.clone()
        };
        let repo_note = if config.repo_dir.is_some() {
            "\nYour working directory is a checkout of the TranslateMom monorepo. Read whatever files you need to make the brief accurate and concrete.\n"
        } else {
            ""
        };
        let url = format!(
            "{}/{}/browse/{}",
            config.base_url, config.workspace, item.key
        );
        format!(
            "You are writing a task brief for a coding agent that will pick up the Plane work item below and work on it in the TranslateMom monorepo. The agent will see only your brief, not this conversation.\n\
             {repo_note}\n\
             <business_context>\n{context}\n</business_context>\n\n\
             <work_item>\n\
             key: {key}\n\
             project: {project_id} {project_name}\n\
             title: {title}\n\
             state: {state}\n\
             priority: {priority}\n\
             labels: {labels}\n\
             due: {due}\n\
             url: {url}\n\
             description:\n{description}\n\
             </work_item>\n\n\
             Write the brief you would want to receive if you were the agent taking on this task. Use your judgment about structure, length, and what to include — whatever best serves this particular work item. It is an assignment (what and why), not an implementation plan (how); leave design decisions to the agent doing the work.\n\
             Two things matter: everything in the brief must be true — grounded in the business context or the repository, with open questions flagged as assumptions to verify rather than guessed at — and it must be specific to this work item rather than a restatement of general context.\n\n\
             Output only the final Markdown brief, with no preamble or commentary.\n",
            key = item.key,
            project_id = project.identifier,
            project_name = project.name,
            title = item.title,
            state = item.state.name(),
            priority = item.priority.as_plane(),
            due = item.due.clone().unwrap_or_else(|| "none".to_owned()),
        )
    }

    fn draw(&mut self) -> Result<()> {
        let (width, height) = size()?;
        let mut stdout = io::stdout();
        queue!(stdout, BeginSynchronizedUpdate, Hide)?;
        let size_changed = self.last_size != Some((width, height));
        if self.force_clear || size_changed {
            clear_area(&mut stdout, 0, 0, width, height, Some(BG))?;
            self.force_clear = false;
            self.last_size = Some((width, height));
        }
        let frame = LayoutFrame::new(width, height);
        draw_outer_frame(&mut stdout, frame)?;
        self.draw_header(&mut stdout, frame.x, frame.width, frame.y)?;
        let footer_height = if self.api_open { 8 } else { 3 };
        let body_top = frame.y + 1;
        let body_height = frame.height.saturating_sub(1 + footer_height);
        let inspector_width = if frame.width >= 130 {
            46
        } else if frame.width >= 105 {
            36
        } else {
            0
        };
        let board_width = frame.width.saturating_sub(inspector_width);
        match self.view {
            ViewMode::Board => {
                self.draw_board(&mut stdout, frame.x, body_top, board_width, body_height)?
            }
            ViewMode::List => {
                self.draw_list(&mut stdout, frame.x, body_top, board_width, body_height)?
            }
        }
        if inspector_width > 0 {
            self.draw_inspector(
                &mut stdout,
                frame.x + board_width,
                body_top,
                inspector_width,
                body_height,
            )?;
        }
        self.draw_footer(
            &mut stdout,
            frame.x,
            body_top + body_height,
            frame.width,
            footer_height,
        )?;
        if self.keys_open {
            self.draw_keys_overlay(&mut stdout, width, height)?;
        }
        if self.notes_open {
            self.draw_notes_overlay(&mut stdout, width, height)?;
        }
        if self.triage.is_some() {
            self.draw_triage_overlay(&mut stdout, width, height)?;
        }
        if self.detail.is_some() {
            self.draw_detail_overlay(&mut stdout, width, height)?;
        }
        if self.prompt_view.is_some() {
            self.draw_prompt_overlay(&mut stdout, width, height)?;
        }
        queue!(stdout, ResetColor, EndSynchronizedUpdate)?;
        stdout.flush()?;
        Ok(())
    }

    fn draw_active_overlay(&mut self) -> Result<()> {
        let (width, height) = size()?;
        let mut stdout = io::stdout();
        queue!(stdout, BeginSynchronizedUpdate, Hide)?;
        if self.prompt_view.is_some() {
            self.draw_prompt_overlay_body(&mut stdout, width, height)?;
        } else if self.detail.is_some() {
            self.draw_detail_overlay_body(&mut stdout, width, height)?;
        }
        queue!(stdout, ResetColor, EndSynchronizedUpdate)?;
        stdout.flush()?;
        Ok(())
    }

    fn draw_footer_only(&mut self) -> Result<()> {
        let (width, height) = size()?;
        let mut stdout = io::stdout();
        queue!(stdout, BeginSynchronizedUpdate, Hide)?;
        let frame = LayoutFrame::new(width, height);
        let footer_height = if self.api_open { 8 } else { 3 };
        let body_top = frame.y + 1;
        let body_height = frame.height.saturating_sub(1 + footer_height);
        self.draw_footer(
            &mut stdout,
            frame.x,
            body_top + body_height,
            frame.width,
            footer_height,
        )?;
        queue!(stdout, ResetColor, EndSynchronizedUpdate)?;
        stdout.flush()?;
        Ok(())
    }

    fn can_redraw_footer_only_after_key(
        &self,
        input_mode_before: Option<InputMode>,
        force_clear_before: bool,
        codex_redraw: bool,
    ) -> bool {
        if force_clear_before || self.force_clear || codex_redraw {
            return false;
        }
        if self.keys_open
            || self.notes_open
            || self.triage.is_some()
            || self.detail.is_some()
            || self.prompt_view.is_some()
        {
            return false;
        }

        let Some(previous_mode) = input_mode_before else {
            return false;
        };
        let Some(current_mode) = self.input_mode else {
            return false;
        };

        previous_mode.can_redraw_footer_only() && current_mode.can_redraw_footer_only()
    }

    fn draw_header(&self, out: &mut io::Stdout, start_x: u16, width: u16, y: u16) -> Result<()> {
        draw_cell(out, start_x, y, width, "", DIM, Some(BG), false)?;
        let mut x = start_x;
        draw_span(
            out,
            &mut x,
            y,
            " plane-tui ",
            Color::Black,
            Some(ACCENT),
            true,
        )?;
        draw_text(
            out,
            &mut x,
            y,
            &format!(" {} │ ", self.client.config.workspace),
            DIM,
        )?;
        for (index, project) in self.projects.iter().enumerate() {
            let tab = format!("{}:{} {} ", index + 1, project.identifier, project.name);
            if index == self.active_project {
                draw_span(out, &mut x, y, &tab, Color::Black, Some(PAPER), true)?;
            } else {
                draw_text(out, &mut x, y, &tab, DIM)?;
            }
            draw_text(out, &mut x, y, "· ", LINE)?;
        }
        let host = self
            .client
            .config
            .base_url
            .replace("https://", "")
            .replace("http://", "");
        let mut right_segments: Vec<(String, Color, bool)> = Vec::new();
        if !self.search.is_empty() {
            right_segments.push((format!("/{}", self.search), ACCENT, false));
        }
        if self.filter != FilterMode::All {
            right_segments.push((format!("f:{}", self.filter.label()), ACCENT, false));
        }
        right_segments.push((format!("sort:{}", self.sort.label()), DIM, false));
        if self.busy.is_some() {
            right_segments.push((format!("J 1 {}", FRAMES[self.frame]), AMBER, true));
        } else {
            right_segments.push(("J 0".to_owned(), DIMMER, false));
        }
        let sync_secs = self.project().loaded_at.elapsed().as_secs();
        let (sync_text, sync_color) = if sync_secs < 60 {
            ("⟳now".to_owned(), DIMMER)
        } else if sync_secs < 3600 {
            (
                format!("⟳{}m", sync_secs / 60),
                if sync_secs >= 900 { AMBER } else { DIMMER },
            )
        } else {
            (format!("⟳{}h", sync_secs / 3600), AMBER)
        };
        right_segments.push((sync_text, sync_color, false));
        right_segments.push((host, DIM, false));
        right_segments.push(("●".to_owned(), GREEN, false));

        let right_width = right_segments
            .iter()
            .map(|(text, _, _)| text.width())
            .sum::<usize>()
            + right_segments.len().saturating_sub(1);
        if width as usize > right_width + 1 {
            let mut right_x = start_x + width.saturating_sub(right_width as u16 + 1);
            if right_x > x.saturating_add(1) {
                for (index, (text, color, bold)) in right_segments.iter().enumerate() {
                    if index > 0 {
                        draw_span(out, &mut right_x, y, " ", DIMMER, Some(BG), false)?;
                    }
                    draw_span(out, &mut right_x, y, text, *color, Some(BG), *bold)?;
                }
            }
        }
        Ok(())
    }

    fn draw_board(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<()> {
        let states = self.board_states();
        let col_widths = distribute_width(width, states.len(), 20);
        let mut col_x = x;
        for (col_index, state) in states.iter().enumerate() {
            let effective_width = col_widths[col_index];
            if col_x >= x + width {
                continue;
            }
            if effective_width < 8 {
                col_x = col_x.saturating_add(effective_width);
                continue;
            }
            let total = self.project().total_for(*state);
            let indices = self.filtered_indices_for_state(*state);
            let shown = if col_index == self.column && !indices.is_empty() {
                format!(" {}/{}", self.row.min(indices.len() - 1) + 1, indices.len())
            } else {
                String::new()
            };
            draw_cell(out, col_x, y, effective_width, "", DIM, Some(BG), false)?;
            let mut header_x = col_x + 1;
            draw_span(
                out,
                &mut header_x,
                y,
                state.glyph(),
                state.color(),
                Some(BG),
                true,
            )?;
            draw_span(out, &mut header_x, y, " ", DIM, Some(BG), false)?;
            let wip_limit = self.wip_limit();
            let over_wip = *state == StateKind::Started && wip_limit > 0 && total > wip_limit;
            draw_span(
                out,
                &mut header_x,
                y,
                state.name(),
                if over_wip { RED } else { PAPER },
                Some(BG),
                true,
            )?;
            draw_span(out, &mut header_x, y, " ", DIM, Some(BG), false)?;
            let count_text = if *state == StateKind::Started && wip_limit > 0 {
                format!("{total}/{wip_limit}")
            } else {
                total.to_string()
            };
            draw_span(
                out,
                &mut header_x,
                y,
                &count_text,
                if over_wip { RED } else { DIM },
                Some(BG),
                over_wip,
            )?;
            if !shown.is_empty() && effective_width as usize > shown.width() + 1 {
                draw_cell(
                    out,
                    col_x + effective_width.saturating_sub(shown.width() as u16 + 1),
                    y,
                    shown.width() as u16,
                    shown.trim(),
                    DIMMER,
                    Some(BG),
                    false,
                )?;
            }
            queue!(
                out,
                MoveTo(col_x, y + 1),
                SetForegroundColor(LINE),
                SetBackgroundColor(BG),
                Print("─".repeat(effective_width.saturating_sub(1) as usize))
            )?;
            for row in 0..height {
                queue!(
                    out,
                    MoveTo(col_x + effective_width.saturating_sub(1), y + row),
                    SetForegroundColor(LINE),
                    SetBackgroundColor(BG),
                    Print("│")
                )?;
            }
            let max_cards = height.saturating_sub(3) as usize / CARD_HEIGHT as usize;
            let selected_row = if col_index == self.column {
                self.row
            } else {
                usize::MAX
            };
            let window_start = if col_index == self.column && max_cards > 0 {
                selected_row.saturating_sub(max_cards.saturating_sub(1))
            } else {
                0
            };
            for (visible, item_index) in indices
                .iter()
                .skip(window_start)
                .take(max_cards)
                .enumerate()
            {
                let absolute_row = window_start + visible;
                let item = &self.project().items[*item_index];
                let card_y = y + 2 + visible as u16 * CARD_HEIGHT;
                self.draw_card(
                    out,
                    col_x + 1,
                    card_y,
                    effective_width.saturating_sub(3),
                    item,
                    absolute_row == selected_row,
                )?;
            }
            if indices.len() > max_cards && height > 2 {
                let hidden_before = window_start;
                let hidden_after = indices.len().saturating_sub(window_start + max_cards);
                let more = if hidden_before > 0 && hidden_after > 0 {
                    format!("… {hidden_before} above · {hidden_after} below · R fetch")
                } else if hidden_before > 0 {
                    format!("… {hidden_before} above · R fetch")
                } else {
                    format!("… {hidden_after} more · R fetch")
                };
                draw_cell(
                    out,
                    col_x,
                    y + height.saturating_sub(1),
                    effective_width.saturating_sub(1),
                    &more,
                    DIM,
                    Some(BG),
                    false,
                )?;
            }
            col_x = col_x.saturating_add(effective_width);
        }
        Ok(())
    }

    fn draw_card(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        item: &WorkItem,
        selected: bool,
    ) -> Result<()> {
        let fg = if selected { Color::Black } else { PAPER };
        let marked = if self.marks.contains(&item.key) {
            "✓"
        } else {
            " "
        };
        let border_color = if selected { ACCENT } else { LINE };
        draw_card_border(out, x, y, width, border_color, Some(CELL_BG))?;
        let inner_x = x + 1;
        let inner_width = width.saturating_sub(2);
        draw_cell(
            out,
            inner_x,
            y + 1,
            inner_width,
            "",
            DIM,
            Some(CELL_BG),
            false,
        )?;
        let mut cursor = inner_x;
        draw_span(out, &mut cursor, y + 1, marked, ACCENT, Some(CELL_BG), true)?;
        draw_span(out, &mut cursor, y + 1, " ", DIM, Some(CELL_BG), false)?;
        draw_span(
            out,
            &mut cursor,
            y + 1,
            &item.key,
            DIM,
            Some(CELL_BG),
            false,
        )?;
        let glyph = item.priority.glyph();
        let glyph_width = glyph.width() as u16;
        if inner_width > glyph_width {
            draw_cell(
                out,
                inner_x + inner_width.saturating_sub(glyph_width),
                y + 1,
                glyph_width,
                glyph,
                item.priority.color(),
                Some(CELL_BG),
                true,
            )?;
        }
        let title_bg = if selected {
            Some(ACCENT)
        } else {
            Some(CELL_BG)
        };
        let title_lines = wrap_line(&item.title, inner_width as usize);
        draw_cell(
            out,
            inner_x,
            y + 2,
            inner_width,
            title_lines.first().map(String::as_str).unwrap_or(""),
            fg,
            title_bg,
            selected,
        )?;
        let title2 = title_lines.get(1).cloned().unwrap_or_default();
        draw_cell(
            out,
            inner_x,
            y + 3,
            inner_width,
            &title2,
            fg,
            title_bg,
            selected,
        )?;
        let (meta, meta_color) = match card_due_alert(item.due.as_deref()) {
            Some((alert, color)) => (alert, color),
            None => (
                item.updated_at
                    .map(time_ago)
                    .unwrap_or_else(|| "unknown".to_owned()),
                DIMMER,
            ),
        };
        self.draw_card_labels(out, inner_x, y + 4, inner_width, item, &meta, meta_color)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_card_labels(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        item: &WorkItem,
        age: &str,
        age_color: Color,
    ) -> Result<()> {
        draw_cell(out, x, y, width, "", DIM, Some(CELL_BG), false)?;
        let age_width = age.width().min(width as usize);
        if age_width < width as usize {
            draw_cell(
                out,
                x + width.saturating_sub(age_width as u16),
                y,
                age_width as u16,
                age,
                age_color,
                Some(CELL_BG),
                false,
            )?;
        }

        let label_width = width.saturating_sub(age_width as u16 + 1);
        let mut cursor = x;
        let cell_end = x.saturating_add(label_width);
        let mut rendered = 0;
        for label_id in item.label_ids.iter().take(2) {
            let Some(label) = self
                .project()
                .labels
                .iter()
                .find(|label| &label.id == label_id)
            else {
                continue;
            };
            if cursor.saturating_sub(x) >= label_width {
                break;
            }
            if rendered > 0 {
                draw_span_clipped(
                    out,
                    &mut cursor,
                    cell_end,
                    y,
                    " ",
                    DIM,
                    Some(CELL_BG),
                    false,
                )?;
            }
            if cursor >= cell_end {
                break;
            }
            let text = format!("{}{}", color_marker(label.color), label.name);
            draw_span_clipped(
                out,
                &mut cursor,
                cell_end,
                y,
                &text,
                label.color,
                Some(CELL_BG),
                false,
            )?;
            rendered += 1;
        }
        if rendered == 0 {
            if item.labels.is_empty() {
                draw_cell(
                    out,
                    x,
                    y,
                    label_width,
                    "no labels",
                    DIM,
                    Some(CELL_BG),
                    false,
                )?;
            } else {
                let fallback = item
                    .labels
                    .iter()
                    .take(2)
                    .map(|label| format!("·{label}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                draw_cell(out, x, y, label_width, &fallback, DIM, Some(CELL_BG), false)?;
            }
        }
        Ok(())
    }

    fn draw_list(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }

        clear_area(out, x, y, width, height, Some(BG))?;
        let layout = ListLayout::new(width);
        draw_cell(out, x, y, width, "", DIMMER, Some(BG), false)?;
        self.draw_list_header(out, x, y, width, layout)?;
        if height > 1 {
            draw_cell(
                out,
                x,
                y + 1,
                width,
                &"─".repeat(width as usize),
                LINE,
                Some(BG),
                false,
            )?;
        }

        let indices = self.flat_indices();
        if indices.is_empty() || height <= 2 {
            return Ok(());
        }

        let row_area = height.saturating_sub(2) as usize;
        let needs_footer = indices.len() > row_area;
        let visible_rows = if needs_footer {
            height.saturating_sub(3) as usize
        } else {
            row_area
        };
        if visible_rows == 0 {
            let hidden = indices.len();
            draw_cell(
                out,
                x,
                y + height.saturating_sub(1),
                width,
                &format!("… {hidden} below"),
                DIMMER,
                Some(BG),
                false,
            )?;
            return Ok(());
        }

        let selected_row = self.cursor.min(indices.len().saturating_sub(1));
        let window_start = selected_row.saturating_sub(visible_rows.saturating_sub(1));
        for (visible_row, index) in indices
            .iter()
            .skip(window_start)
            .take(visible_rows)
            .enumerate()
        {
            let item = &self.project().items[*index];
            let absolute_row = window_start + visible_row;
            let selected = absolute_row == selected_row;
            self.draw_list_row(
                out,
                x,
                y + 2 + visible_row as u16,
                width,
                layout,
                item,
                selected,
            )?;
        }

        if needs_footer {
            let hidden_above = window_start;
            let hidden_below = indices
                .len()
                .saturating_sub(window_start.saturating_add(visible_rows));
            let footer = match (hidden_above, hidden_below) {
                (0, below) => format!("… {below} below"),
                (above, 0) => format!("… {above} above"),
                (above, below) => format!("… {above} above · {below} below"),
            };
            draw_cell(
                out,
                x,
                y + height.saturating_sub(1),
                width,
                &footer,
                DIMMER,
                Some(BG),
                false,
            )?;
        }
        Ok(())
    }

    fn draw_list_header(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        layout: ListLayout,
    ) -> Result<()> {
        let bg = Some(BG);
        let mut cursor = x;
        let end = x.saturating_add(width);
        cursor = draw_list_cell(out, cursor, end, y, layout.mark, "", DIMMER, bg, false)?;
        cursor = draw_list_cell(out, cursor, end, y, layout.priority, "p", DIMMER, bg, false)?;
        cursor = draw_list_cell(out, cursor, end, y, layout.key, "key", DIMMER, bg, false)?;
        cursor = draw_list_cell(
            out,
            cursor,
            end,
            y,
            layout.title,
            "title",
            DIMMER,
            bg,
            false,
        )?;
        cursor = draw_list_cell(
            out,
            cursor,
            end,
            y,
            layout.state,
            "state",
            DIMMER,
            bg,
            false,
        )?;
        cursor = draw_list_cell(
            out,
            cursor,
            end,
            y,
            layout.labels,
            "labels",
            DIMMER,
            bg,
            false,
        )?;
        cursor = draw_list_cell_right(out, cursor, end, y, layout.due, "due", DIMMER, bg, false)?;
        let _ = draw_list_cell_right(
            out,
            cursor,
            end,
            y,
            layout.updated,
            "updated",
            DIMMER,
            bg,
            false,
        )?;
        Ok(())
    }

    fn draw_list_row(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        layout: ListLayout,
        item: &WorkItem,
        selected: bool,
    ) -> Result<()> {
        let bg = if selected { Some(ACCENT) } else { Some(BG) };
        let selected_fg = Color::Black;
        draw_cell(out, x, y, width, "", selected_fg, bg, false)?;

        let mark_fg = if selected { selected_fg } else { ACCENT };
        let priority_fg = if selected {
            selected_fg
        } else {
            item.priority.color()
        };
        let key_fg = if selected { selected_fg } else { DIM };
        let title_fg = if selected { selected_fg } else { PAPER };
        let state_fg = if selected {
            selected_fg
        } else {
            item.state.color()
        };
        let muted_fg = if selected { selected_fg } else { DIMMER };
        let (due, due_fg) = list_due(item.due.as_deref());
        let due_fg = if selected { selected_fg } else { due_fg };
        let updated = item
            .updated_at
            .map(time_ago)
            .unwrap_or_else(|| "-".to_owned());
        let mark = if self.marks.contains(&item.key) {
            "✓"
        } else {
            " "
        };

        let mut cursor = x;
        let end = x.saturating_add(width);
        cursor = draw_list_cell(out, cursor, end, y, layout.mark, mark, mark_fg, bg, true)?;
        cursor = draw_list_cell(
            out,
            cursor,
            end,
            y,
            layout.priority,
            item.priority.glyph(),
            priority_fg,
            bg,
            true,
        )?;
        cursor = draw_list_cell(
            out, cursor, end, y, layout.key, &item.key, key_fg, bg, false,
        )?;
        cursor = draw_list_cell(
            out,
            cursor,
            end,
            y,
            layout.title,
            &item.title,
            title_fg,
            bg,
            false,
        )?;
        let state = format!(
            "{} {}",
            item.state.glyph(),
            item.state.name().to_lowercase()
        );
        cursor = draw_list_cell(
            out,
            cursor,
            end,
            y,
            layout.state,
            &state,
            state_fg,
            bg,
            false,
        )?;
        cursor = self.draw_list_labels(out, cursor, end, y, layout.labels, item, bg, selected)?;
        cursor = draw_list_cell_right(out, cursor, end, y, layout.due, &due, due_fg, bg, false)?;
        let _ = draw_list_cell_right(
            out,
            cursor,
            end,
            y,
            layout.updated,
            &updated,
            muted_fg,
            bg,
            false,
        )?;
        Ok(())
    }

    fn draw_list_labels(
        &self,
        out: &mut io::Stdout,
        x: u16,
        end: u16,
        y: u16,
        width: u16,
        item: &WorkItem,
        bg: Option<Color>,
        selected: bool,
    ) -> Result<u16> {
        let effective_width = width.min(end.saturating_sub(x));
        if effective_width == 0 {
            return Ok(next_list_x(x, width, end));
        }

        let fg = if selected { Color::Black } else { DIM };
        draw_cell(out, x, y, effective_width, "", fg, bg, false)?;
        let mut cursor = x;
        let cell_end = x.saturating_add(effective_width);
        let mut rendered = 0;
        for label_id in item.label_ids.iter().take(2) {
            let Some(label) = self
                .project()
                .labels
                .iter()
                .find(|label| &label.id == label_id)
            else {
                continue;
            };
            if cursor >= cell_end {
                break;
            }
            if rendered > 0 {
                draw_span_clipped(out, &mut cursor, cell_end, y, " ", fg, bg, false)?;
            }
            let label_fg = if selected { Color::Black } else { label.color };
            let text = format!("{}{}", color_marker(label.color), label.name);
            draw_span_clipped(out, &mut cursor, cell_end, y, &text, label_fg, bg, false)?;
            rendered += 1;
        }

        if rendered == 0 && !item.labels.is_empty() {
            let fallback = item
                .labels
                .iter()
                .take(2)
                .map(|label| format!("·{}", label))
                .collect::<Vec<_>>()
                .join(" ");
            draw_span_clipped(out, &mut cursor, cell_end, y, &fallback, fg, bg, false)?;
        }

        Ok(next_list_x(x, width, end))
    }

    fn draw_inspector(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<()> {
        let content_x = x.saturating_add(1);
        let content_width = width.saturating_sub(1);
        clear_area(out, content_x, y, content_width, height, Some(BG))?;
        for row in 0..height {
            queue!(
                out,
                MoveTo(x, y + row),
                SetForegroundColor(LINE),
                SetBackgroundColor(BG),
                Print("│")
            )?;
        }
        let Some(item) = self.current_item() else {
            draw_cell(
                out,
                content_x,
                y,
                content_width,
                "no item",
                DIM,
                None,
                false,
            )?;
            return Ok(());
        };
        let mut row = y;
        draw_cell(out, content_x, row, content_width, "", DIM, None, false)?;
        let mut cursor = content_x;
        draw_span(out, &mut cursor, row, &item.key, DIM, Some(BG), true)?;
        draw_span(out, &mut cursor, row, " · ", DIMMER, Some(BG), false)?;
        draw_span(
            out,
            &mut cursor,
            row,
            &format!("{} {}", item.priority.glyph(), item.priority.as_plane()),
            item.priority.color(),
            Some(BG),
            true,
        )?;
        let state_text = format!(
            "{} {}",
            item.state.glyph(),
            item.state.name().to_lowercase()
        );
        let state_width = state_text.width() as u16;
        if content_width > state_width {
            draw_cell(
                out,
                content_x + content_width.saturating_sub(state_width),
                row,
                state_width,
                &state_text,
                item.state.color(),
                Some(BG),
                true,
            )?;
        }
        row += 1;
        for line in wrap_line(&item.title, width.saturating_sub(3) as usize)
            .into_iter()
            .take(2)
        {
            draw_cell(out, x + 1, row, width - 1, &line, PAPER, None, true)?;
            row += 1;
        }
        if row < y + height {
            draw_cell(
                out,
                x + 1,
                row,
                width - 1,
                &"─".repeat(width.saturating_sub(2) as usize),
                LINE,
                None,
                false,
            )?;
            row += 1;
        }
        let fields = [
            (
                "state",
                format!(
                    "{} {} · s to move",
                    item.state.glyph(),
                    self.project().state_name(&item.state_id)
                ),
                item.state.color(),
            ),
            (
                "priority",
                format!("{} · p to set", item.priority.as_plane()),
                item.priority.color(),
            ),
            ("labels", String::new(), TEXT),
            (
                "due",
                match item.due.as_deref() {
                    Some(due) => match card_due_alert(Some(due)) {
                        Some((alert, _)) => format!("{due} · {alert}"),
                        None => due.to_owned(),
                    },
                    None => "none · d to set".to_owned(),
                },
                match item.due.as_deref() {
                    Some(due) => card_due_alert(Some(due))
                        .map(|(_, color)| color)
                        .unwrap_or(TEXT),
                    None => DIMMER,
                },
            ),
            (
                "created",
                item.created_at
                    .map(|dt| dt.date_naive().to_string())
                    .unwrap_or_else(|| "unknown".to_owned()),
                TEXT,
            ),
            (
                "updated",
                item.updated_at
                    .map(|dt| format!("{} · {}", dt.date_naive(), time_ago(dt)))
                    .unwrap_or_else(|| "unknown".to_owned()),
                TEXT,
            ),
            (
                "completed",
                item.completed_at
                    .clone()
                    .map(|value| value.chars().take(10).collect::<String>())
                    .unwrap_or_else(|| "none".to_owned()),
                TEXT,
            ),
            (
                "url",
                format!(
                    "{}/{}/browse/{}",
                    self.client.config.base_url, self.client.config.workspace, item.key
                ),
                ACCENT,
            ),
        ];
        for (name, value, value_color) in fields {
            if row >= y + height {
                return Ok(());
            }
            if name == "labels" {
                draw_label_field(out, x + 1, row, width - 1, self.project(), item)?;
            } else if name == "url" {
                draw_link_field(out, x + 1, row, width - 1, &value, value_color)?;
            } else {
                draw_field_line(out, x + 1, row, width - 1, name, &value, value_color)?;
            }
            row += 1;
        }
        row += 1;
        draw_cell(out, x + 1, row, width - 1, "description", DIM, None, false)?;
        row += 1;
        let desc = if item.description.trim().is_empty() {
            "(no description · e to edit)".to_owned()
        } else {
            item.description.clone()
        };
        for line in wrap_line(&desc, width.saturating_sub(3) as usize)
            .into_iter()
            .take(height.saturating_sub(row - y + 4) as usize)
        {
            draw_cell(out, x + 1, row, width - 1, &line, DIM, None, false)?;
            row += 1;
        }
        if !item.actions.is_empty() && row + 1 < y + height {
            row += 1;
            draw_cell(out, x + 1, row, width - 1, "activity", DIM, None, false)?;
            row += 1;
            for action in item.actions.iter().take(4) {
                if row >= y + height {
                    break;
                }
                draw_cell(out, x + 1, row, width - 1, action, DIM, None, false)?;
                row += 1;
            }
        }
        Ok(())
    }

    fn draw_footer(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<()> {
        queue!(
            out,
            MoveTo(x, y),
            SetForegroundColor(LINE),
            SetBackgroundColor(BG),
            Print("─".repeat(width as usize))
        )?;
        let mut row = y + 1;
        let pad = if width > 2 { 1 } else { 0 };
        let inner_x = x + pad;
        let inner_width = width.saturating_sub(pad * 2);
        if self.api_open && height > 4 {
            draw_cell(
                out,
                inner_x,
                row,
                inner_width,
                &format!(
                    "api · base {} · workspace {} · auth X-API-Key **** · x to close",
                    self.client.config.base_url, self.client.config.workspace
                ),
                DIM,
                None,
                false,
            )?;
            row += 1;
            for entry in self
                .api_log
                .iter()
                .rev()
                .take(height.saturating_sub(4) as usize)
            {
                draw_cell(
                    out,
                    inner_x,
                    row,
                    inner_width,
                    &format!(
                        "{:<8} {:<5} {:<48} {:<20} {:>4} {:>5}ms",
                        entry.time,
                        entry.method,
                        truncate(&entry.path, 48),
                        truncate(&entry.payload, 20),
                        entry.status,
                        entry.ms
                    ),
                    match entry.method {
                        "PATCH" => AMBER,
                        "POST" => GREEN,
                        "GET" => ACCENT,
                        _ => TEXT,
                    },
                    None,
                    false,
                )?;
                row += 1;
            }
        }
        if self.backend_wizard.is_some() && row < y + height {
            self.draw_backend_wizard_bar(out, inner_x, row, inner_width)?;
            row += 1;
        } else {
            let label_menu_open = self.menu == Some(MenuMode::Label);
            if label_menu_open && row < y + height {
                self.draw_label_menu_bar(out, inner_x, row, inner_width)?;
                row += 1;
            } else if !self.marks.is_empty() && row < y + height {
                draw_cell(
                    out,
                    x,
                    row,
                    width,
                    &format!(
                        " {} marked · s state · p priority · t label · o open · I invert · U clear",
                        self.marks.len()
                    ),
                    Color::Black,
                    Some(ACCENT),
                    true,
                )?;
                row += 1;
            }
            if let Some(menu) = self.menu {
                let text = match menu {
                    MenuMode::State => {
                        "state → 1 backlog  2 todo  3 in-progress  4 done  5 cancelled  esc cancel"
                            .to_owned()
                    }
                    MenuMode::Priority => format!(
                        "priority → {}  esc cancel",
                        PRIORITY_ORDER
                            .iter()
                            .map(|priority| format!(
                                "{} {} {}",
                                priority.as_plane().chars().next().unwrap_or('n'),
                                priority.glyph(),
                                priority.as_plane()
                            ))
                            .collect::<Vec<_>>()
                            .join("  ")
                    ),
                    MenuMode::Label => String::new(),
                    MenuMode::Edit => {
                        "edit → t title  d description in $EDITOR  u due date  esc cancel"
                            .to_owned()
                    }
                    MenuMode::ConfirmWip => format!(
                        "In Progress is full ({}/{}) — move anyway? Something must leave first · y move  esc cancel",
                        self.project().total_for(StateKind::Started),
                        self.wip_limit()
                    ),
                };
                if menu != MenuMode::Label {
                    let menu_bg = if menu == MenuMode::ConfirmWip {
                        RED
                    } else {
                        PAPER
                    };
                    draw_cell(
                        out,
                        inner_x,
                        row,
                        inner_width,
                        &text,
                        Color::Black,
                        Some(menu_bg),
                        true,
                    )?;
                    row += 1;
                }
            }
        }
        if row < y + height {
            self.draw_command_line(out, inner_x, row, inner_width)?;
            row += 1;
        }
        if row < y + height {
            draw_cell(
                out,
                inner_x,
                row,
                inner_width,
                "j/k h/l move · enter detail · e edit · a/A agent · m mark · s state · p priority · t label · D done · T triage · v view · / search · : cmd · x api · ? keys · q quit",
                DIMMER,
                None,
                false,
            )?;
        }
        Ok(())
    }

    fn draw_command_line(&self, out: &mut io::Stdout, x: u16, y: u16, width: u16) -> Result<()> {
        if width == 0 {
            return Ok(());
        }

        draw_cell(out, x, y, width, "", TEXT, Some(BG), false)?;

        let left = self.command_line_text();
        let left_color = if self.input_mode.is_some() {
            PAPER
        } else {
            DIM
        };
        let position = self.position_text();
        let position_width = min(position.width(), width as usize) as u16;

        if let Some(busy) = &self.busy {
            let max_right = width.saturating_sub(1);
            let max_job_width = max_right
                .saturating_sub(position_width.saturating_add(2))
                .min(52);
            let job = if max_job_width > 0 {
                truncate(
                    &format!("{} {}", FRAMES[self.frame], busy),
                    max_job_width as usize,
                )
            } else {
                String::new()
            };
            let job_width = job.width() as u16;
            let gap = if job_width > 0 && position_width > 0 {
                2
            } else {
                0
            };
            let right_width = job_width
                .saturating_add(gap)
                .saturating_add(position_width)
                .min(width);
            let left_width = width.saturating_sub(right_width.saturating_add(1));

            draw_cell(out, x, y, left_width, &left, left_color, Some(BG), false)?;

            let mut cursor = x + width.saturating_sub(right_width);
            if job_width > 0 {
                draw_cell(out, cursor, y, job_width, &job, AMBER, Some(BG), true)?;
                cursor = cursor.saturating_add(job_width).saturating_add(gap);
            }
            if position_width > 0 && cursor < x + width {
                draw_cell(
                    out,
                    cursor,
                    y,
                    min(position_width, x + width - cursor),
                    &position,
                    DIMMER,
                    Some(BG),
                    false,
                )?;
            }
            return Ok(());
        }

        let right_width = if position_width > 0 {
            position_width.saturating_add(2).min(width)
        } else {
            0
        };
        let left_width = width.saturating_sub(right_width);
        draw_cell(out, x, y, left_width, &left, left_color, Some(BG), false)?;
        if position_width > 0 {
            draw_cell(
                out,
                x + width.saturating_sub(position_width),
                y,
                position_width,
                &position,
                DIMMER,
                Some(BG),
                false,
            )?;
        }
        Ok(())
    }

    fn command_line_text(&self) -> String {
        if let Some(mode) = self.input_mode {
            let prefix = match mode {
                InputMode::Search => "/",
                InputMode::Command => ":",
                InputMode::NewLabel => "new label → ",
                InputMode::BackendModel => "claude model → ",
                InputMode::BackendEffort => "claude effort → ",
                InputMode::ProjectName => "new project name → ",
                InputMode::ProjectIdentifier => "new project key → ",
                InputMode::EditTitle => "edit title → ",
                InputMode::EditDue => "edit due → ",
            };
            input_prompt(prefix, &self.input, self.input_cursor)
        } else {
            self.status.clone()
        }
    }

    fn position_text(&self) -> String {
        match self.view {
            ViewMode::Board => {
                let states = self.board_states();
                let state = states[self.column];
                let len = self.filtered_indices_for_state(state).len();
                format!(
                    "{} / {} · {}",
                    min(self.row + 1, len),
                    len,
                    state.name().to_lowercase()
                )
            }
            ViewMode::List => {
                let len = self.flat_indices().len();
                format!("{} / {} · list", min(self.cursor + 1, len), len)
            }
        }
    }

    fn draw_backend_wizard_bar(
        &self,
        out: &mut io::Stdout,
        x: u16,
        y: u16,
        width: u16,
    ) -> Result<()> {
        let Some(wizard) = self.backend_wizard.as_ref() else {
            return Ok(());
        };
        let end = x.saturating_add(width);
        draw_cell(out, x, y, width, "", DIM, Some(BG_RAISE), false)?;
        let mut cursor = x;
        draw_span_clipped(
            out,
            &mut cursor,
            end,
            y,
            " backend → ",
            Color::Black,
            Some(PAPER),
            true,
        )?;
        self.draw_backend_wizard_choice(
            out,
            &mut cursor,
            end,
            y,
            "1 codex",
            wizard.selected == AgentBackend::Codex,
        )?;
        self.draw_backend_wizard_choice(
            out,
            &mut cursor,
            end,
            y,
            "2 claude",
            wizard.selected == AgentBackend::Claude,
        )?;
        let details = format!(
            "  model {}  effort {}",
            wizard.claude_model, wizard.claude_effort
        );
        draw_span_clipped(
            out,
            &mut cursor,
            end,
            y,
            &details,
            DIM,
            Some(BG_RAISE),
            false,
        )?;
        draw_span_clipped(
            out,
            &mut cursor,
            end,
            y,
            "  m model  e effort  enter save  esc cancel",
            DIMMER,
            Some(BG_RAISE),
            false,
        )?;
        Ok(())
    }

    fn draw_backend_wizard_choice(
        &self,
        out: &mut io::Stdout,
        cursor: &mut u16,
        end: u16,
        y: u16,
        label: &str,
        selected: bool,
    ) -> Result<()> {
        draw_span_clipped(out, cursor, end, y, " ", DIM, Some(BG_RAISE), false)?;
        let bg = if selected { ACCENT } else { CELL_BG };
        let fg = if selected { Color::Black } else { PAPER };
        draw_span_clipped(
            out,
            cursor,
            end,
            y,
            &format!(" {label} "),
            fg,
            Some(bg),
            selected,
        )
    }

    fn draw_label_menu_bar(&self, out: &mut io::Stdout, x: u16, y: u16, width: u16) -> Result<()> {
        draw_cell(out, x, y, width, "", DIM, Some(BG_RAISE), false)?;
        let mut cursor = x;
        draw_span(
            out,
            &mut cursor,
            y,
            &format!(" toggle label → {} item ", self.target_keys().len()),
            Color::Black,
            Some(PAPER),
            true,
        )?;
        for (index, label) in self.project().labels.iter().take(9).enumerate() {
            if cursor.saturating_sub(x) >= width.saturating_sub(14) {
                break;
            }
            draw_span(out, &mut cursor, y, " ", DIM, Some(BG_RAISE), false)?;
            draw_span(
                out,
                &mut cursor,
                y,
                &(index + 1).to_string(),
                ACCENT,
                Some(BG_RAISE),
                true,
            )?;
            draw_span(out, &mut cursor, y, " ", DIM, Some(BG_RAISE), false)?;
            let text = format!("{}{}", color_marker(label.color), label.name);
            let remaining = width.saturating_sub(cursor.saturating_sub(x)) as usize;
            draw_span(
                out,
                &mut cursor,
                y,
                &truncate(&text, remaining.min(16)),
                label.color,
                Some(BG_RAISE),
                false,
            )?;
        }
        if cursor.saturating_sub(x) < width.saturating_sub(12) {
            draw_span(out, &mut cursor, y, "  n ", DIM, Some(BG_RAISE), false)?;
            draw_span(
                out,
                &mut cursor,
                y,
                "new label",
                GREEN,
                Some(BG_RAISE),
                true,
            )?;
        }
        if cursor.saturating_sub(x) < width.saturating_sub(10) {
            draw_span(
                out,
                &mut cursor,
                y,
                "  esc done",
                DIM,
                Some(BG_RAISE),
                false,
            )?;
        }
        Ok(())
    }

    fn draw_keys_overlay(&self, out: &mut io::Stdout, width: u16, height: u16) -> Result<()> {
        draw_help_panel(out, width, height)
    }

    fn draw_notes_overlay(&self, out: &mut io::Stdout, width: u16, height: u16) -> Result<()> {
        let lines = [
            " design notes ",
            "The board tells the truth: live Plane items, not a local mock.",
            "Projects are one keystroke away: Product, iOS, Growth.",
            "Cursor is reverse video; marks are checkmarks and drive batch actions.",
            "Triage is one item at a time for unprioritized backlog work.",
            "Every mutation is logged in the API drawer with method, path, status, and latency.",
            "Inspector keeps state, priority, labels, dates, description, activity, and canonical browse URL visible.",
            "esc/q/! closes this overlay",
        ];
        draw_overlay(out, width, height, &lines)
    }

    fn draw_triage_overlay(&self, out: &mut io::Stdout, width: u16, height: u16) -> Result<()> {
        let Some(triage) = self.triage.as_ref() else {
            return Ok(());
        };
        let current = triage
            .keys
            .get(triage.index)
            .and_then(|key| self.find_index_by_key(key))
            .map(|index| &self.project().items[index]);
        let mut lines = vec![
            " triage · backlog sweep ".to_owned(),
            format!(
                "{} / {} decided · {} promoted · {} dropped",
                triage.decided,
                triage.keys.len(),
                triage.promoted,
                triage.dropped
            ),
        ];
        if let Some(item) = current {
            lines.push(format!(
                "{} · no priority · created {}",
                item.key,
                item.created_at
                    .map(time_ago)
                    .unwrap_or_else(|| "unknown".to_owned())
            ));
            lines.extend(wrap_line(&item.title, 72));
            lines.push("priority: u urgent · h high · m medium · l low · n none".to_owned());
            lines.push(
                "state: 2 todo · 3 in-progress · 5 cancelled · enter skip · q finish".to_owned(),
            );
        } else {
            lines.push("loaded page drained · R refresh more · q done".to_owned());
        }
        let refs = lines.iter().map(String::as_str).collect::<Vec<_>>();
        draw_overlay(out, width, height, &refs)
    }

    fn draw_prompt_overlay(&mut self, out: &mut io::Stdout, width: u16, height: u16) -> Result<()> {
        self.draw_prompt_overlay_inner(out, width, height, true)
    }

    fn draw_prompt_overlay_body(
        &mut self,
        out: &mut io::Stdout,
        width: u16,
        height: u16,
    ) -> Result<()> {
        self.draw_prompt_overlay_inner(out, width, height, false)
    }

    fn draw_prompt_overlay_inner(
        &mut self,
        out: &mut io::Stdout,
        width: u16,
        height: u16,
        draw_shell: bool,
    ) -> Result<()> {
        let Some(view) = self.prompt_view.as_mut() else {
            return Ok(());
        };
        let box_width = min(width.saturating_sub(6), 110);
        let box_height = min(height.saturating_sub(4), 44);
        if box_width < 40 || box_height < 8 {
            return draw_overlay(
                out,
                width,
                height,
                &[
                    " agent prompt ",
                    "terminal too small to preview — prompt saved to file and clipboard",
                    "esc/q close",
                ],
            );
        }
        let x = width.saturating_sub(box_width) / 2;
        let y = height.saturating_sub(box_height) / 2;
        let content_width = box_width.saturating_sub(6) as usize;
        let wrapped = view
            .text
            .lines()
            .flat_map(|line| wrap_line(line, content_width))
            .collect::<Vec<_>>();
        let visible = box_height.saturating_sub(5) as usize;
        let max_scroll = wrapped.len().saturating_sub(visible);
        view.scroll = view.scroll.min(max_scroll);
        let scroll = view.scroll;
        let hint = format!(
            "wheel/j/k scroll · y copy · esc close · {}-{}/{} · {}",
            min(scroll + 1, wrapped.len()),
            min(scroll + visible, wrapped.len()),
            wrapped.len(),
            view.file
        );
        if draw_shell {
            let title = format!("agent prompt · {}", view.key);
            draw_modal_shell(out, x, y, box_width, box_height, &title)?;
        }
        for offset in 0..visible {
            let line = wrapped
                .get(scroll + offset)
                .map(String::as_str)
                .unwrap_or("");
            draw_cell(
                out,
                x + 3,
                y + 2 + offset as u16,
                box_width.saturating_sub(6),
                line,
                TEXT,
                Some(BG),
                false,
            )?;
        }
        draw_cell(
            out,
            x + 3,
            y + box_height.saturating_sub(3),
            box_width.saturating_sub(6),
            &hint,
            DIM,
            Some(BG),
            false,
        )?;
        Ok(())
    }

    fn draw_detail_overlay(&mut self, out: &mut io::Stdout, width: u16, height: u16) -> Result<()> {
        self.draw_detail_overlay_inner(out, width, height, true)
    }

    fn draw_detail_overlay_body(
        &mut self,
        out: &mut io::Stdout,
        width: u16,
        height: u16,
    ) -> Result<()> {
        self.draw_detail_overlay_inner(out, width, height, false)
    }

    fn draw_detail_overlay_inner(
        &mut self,
        out: &mut io::Stdout,
        width: u16,
        height: u16,
        draw_shell: bool,
    ) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let detail_key = detail.key.clone();
        let box_width = min(width.saturating_sub(4), 108);
        let box_height = height.saturating_sub(2);
        if box_width < 40 || box_height < 8 {
            return draw_overlay(
                out,
                width,
                height,
                &[" detail ", "terminal too small", "esc/q close"],
            );
        }
        let x = width.saturating_sub(box_width) / 2;
        let y = height.saturating_sub(box_height) / 2;
        let content_width = box_width.saturating_sub(6) as usize;

        let Some(index) = self.find_index_by_key(&detail_key) else {
            return draw_overlay(
                out,
                width,
                height,
                &[" detail ", "item no longer loaded", "esc/q close"],
            );
        };
        let item = &self.project().items[index];

        let mut lines: Vec<(String, Color, bool)> = Vec::new();
        for line in wrap_line(&item.title, content_width) {
            lines.push((line, PAPER, true));
        }
        lines.push((String::new(), TEXT, false));
        lines.push((
            format!(
                "{} {}   ·   {} {}",
                item.state.glyph(),
                self.project().state_name(&item.state_id),
                item.priority.glyph(),
                item.priority.as_plane()
            ),
            item.state.color(),
            false,
        ));
        let labels = if item.labels.is_empty() {
            "none".to_owned()
        } else {
            item.labels.join(" · ")
        };
        lines.push((format!("labels    {labels}"), TEXT, false));
        let (due_text, due_color) = match item.due.as_deref() {
            Some(due) => {
                let (_, color) = list_due(Some(due));
                (due.to_owned(), color)
            }
            None => ("none".to_owned(), DIMMER),
        };
        lines.push((format!("due       {due_text}"), due_color, false));
        lines.push((
            format!(
                "created   {}   ·   updated {}",
                item.created_at
                    .map(|dt| dt.date_naive().to_string())
                    .unwrap_or_else(|| "unknown".to_owned()),
                item.updated_at
                    .map(time_ago)
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
            TEXT,
            false,
        ));
        lines.push((
            format!(
                "url       {}/{}/browse/{}",
                self.client.config.base_url, self.client.config.workspace, item.key
            ),
            ACCENT,
            false,
        ));
        lines.push((String::new(), TEXT, false));
        lines.push(("description".to_owned(), DIM, false));
        let description = if item.description.trim().is_empty() {
            "(no description)".to_owned()
        } else {
            item.description.clone()
        };
        for raw_line in description.lines() {
            for line in wrap_line(raw_line, content_width) {
                lines.push((line, TEXT, false));
            }
        }
        lines.push((String::new(), TEXT, false));
        lines.push((format!("comments ({})", detail.comments.len()), DIM, false));
        if detail.comments.is_empty() {
            lines.push(("(no comments)".to_owned(), DIMMER, false));
        }
        for (when, text) in &detail.comments {
            lines.push((format!("· {when}"), ACCENT, false));
            for raw_line in text.lines() {
                for line in wrap_line(raw_line, content_width.saturating_sub(2)) {
                    lines.push((format!("  {line}"), TEXT, false));
                }
            }
            lines.push((String::new(), TEXT, false));
        }

        let visible = box_height.saturating_sub(5) as usize;
        let max_scroll = lines.len().saturating_sub(visible);
        let scroll = {
            let detail = self.detail.as_mut().expect("detail present");
            detail.scroll = detail.scroll.min(max_scroll);
            detail.scroll
        };
        if draw_shell {
            let title = format!("{detail_key} · detail");
            draw_modal_shell(out, x, y, box_width, box_height, &title)?;
        }
        for offset in 0..visible {
            let (line, color, bold) = lines
                .get(scroll + offset)
                .map(|(line, color, bold)| (line.as_str(), *color, *bold))
                .unwrap_or(("", TEXT, false));
            draw_cell(
                out,
                x + 3,
                y + 2 + offset as u16,
                box_width.saturating_sub(6),
                line,
                color,
                Some(BG),
                bold,
            )?;
        }
        let hint = format!(
            "wheel/j/k scroll · o open · a/A agent prompt · esc close · {}-{}/{}",
            min(scroll + 1, lines.len()),
            min(scroll + visible, lines.len()),
            lines.len()
        );
        draw_cell(
            out,
            x + 3,
            y + box_height.saturating_sub(3),
            box_width.saturating_sub(6),
            &hint,
            DIM,
            Some(BG),
            false,
        )?;
        Ok(())
    }

    fn print_summary(&self) {
        println!("plane-tui connected");
        println!("workspace: {}", self.client.config.workspace);
        println!("base: {}", self.client.config.base_url);
        for project in &self.projects {
            println!(
                "{} {:<8} loaded={} backlog={} todo={} in-progress={} done={}",
                project.identifier,
                project.name,
                project.items.len(),
                project.total_for(StateKind::Backlog),
                project.total_for(StateKind::Todo),
                project.total_for(StateKind::Started),
                project.total_for(StateKind::Done)
            );
        }
        println!("api calls:");
        for entry in &self.api_log {
            println!(
                "{} {:<5} {:<48} {:>4} {}ms",
                entry.time, entry.method, entry.path, entry.status, entry.ms
            );
        }
    }
}

impl ApiLog {
    fn new(method: &'static str, path: &str, payload: &str, status: &str, ms: u128) -> Self {
        Self {
            time: Local::now().format("%H:%M:%S").to_string(),
            method,
            path: path.to_owned(),
            payload: payload.to_owned(),
            status: status.to_owned(),
            ms,
        }
    }
}

#[derive(Clone, Copy)]
struct LayoutFrame {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl LayoutFrame {
    fn new(term_width: u16, term_height: u16) -> Self {
        if term_width < 8 || term_height < 8 {
            return Self {
                x: 0,
                y: 0,
                width: term_width,
                height: term_height,
            };
        }
        Self {
            x: 1,
            y: 1,
            width: term_width.saturating_sub(2),
            height: term_height.saturating_sub(2),
        }
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

fn main() -> Result<()> {
    let config = Config::from_args()?;
    let check_api = config.check_api;
    let client = PlaneClient::new(config);
    let mut app = App::load(client)?;
    if check_api {
        app.print_summary();
        Ok(())
    } else {
        app.run()
    }
}

fn add_clamped(value: usize, delta: isize, max_value: usize) -> usize {
    if delta < 0 {
        value.saturating_sub(delta.unsigned_abs())
    } else {
        min(value.saturating_add(delta as usize), max_value)
    }
}

fn previous_char_boundary(value: &str, index: usize) -> usize {
    let index = index.min(value.len());
    value[..index]
        .char_indices()
        .last()
        .map(|(position, _)| position)
        .unwrap_or(0)
}

fn next_char_boundary(value: &str, index: usize) -> usize {
    let index = index.min(value.len());
    if index >= value.len() {
        return value.len();
    }
    value[index..]
        .char_indices()
        .nth(1)
        .map(|(position, _)| index + position)
        .unwrap_or(value.len())
}

fn input_prompt(prefix: &str, input: &str, cursor: usize) -> String {
    let cursor = cursor.min(input.len());
    let mut prompt = String::with_capacity(prefix.len() + input.len() + "█".len());
    prompt.push_str(prefix);
    prompt.push_str(&input[..cursor]);
    prompt.push('█');
    prompt.push_str(&input[cursor..]);
    prompt
}

fn card_due_alert(due: Option<&str>) -> Option<(String, Color)> {
    let due = due?.trim();
    let date = NaiveDate::parse_from_str(due, "%Y-%m-%d").ok()?;
    let days = date
        .signed_duration_since(Local::now().date_naive())
        .num_days();
    match days {
        d if d < 0 => Some((format!("{}d over", -d), RED)),
        0 => Some(("due today".to_owned(), RED)),
        1 => Some(("due tom".to_owned(), AMBER)),
        2..=3 => Some((format!("due {}", due.get(5..).unwrap_or(due)), AMBER)),
        _ => None,
    }
}

fn project_from_api(
    api_project: ApiProject,
    api_states: Vec<ApiState>,
    api_labels: Vec<ApiLabel>,
    api_items: Vec<ApiItem>,
) -> Project {
    let identifier = api_project.identifier;
    let states = api_states
        .into_iter()
        .map(|state| State {
            id: state.id,
            name: state.name,
            kind: StateKind::from_group(&state.group),
        })
        .collect::<Vec<_>>();
    let state_lookup = states
        .iter()
        .map(|state| (state.id.clone(), state.kind))
        .collect::<BTreeMap<_, _>>();
    let labels = api_labels
        .into_iter()
        .map(|label| Label {
            id: label.id,
            name: label.name,
            color: parse_hex_color(label.color.as_deref().unwrap_or("#777777")),
        })
        .collect::<Vec<_>>();
    let label_lookup = labels
        .iter()
        .map(|label| (label.id.clone(), label.name.clone()))
        .collect::<BTreeMap<_, _>>();
    let items = api_items
        .into_iter()
        .filter(|item| item.archived_at.is_none())
        .map(|item| {
            let state_id = item.state_id.or(item.state).unwrap_or_default();
            let state = state_lookup
                .get(&state_id)
                .copied()
                .unwrap_or(StateKind::Backlog);
            let mut label_ids = item.label_ids;
            if label_ids.is_empty() {
                label_ids = item.labels.clone();
            }
            let mut label_names = item
                .label_details
                .iter()
                .map(|label| label.name.clone())
                .collect::<Vec<_>>();
            if label_names.is_empty() {
                label_names = label_ids
                    .iter()
                    .filter_map(|id| label_lookup.get(id).cloned())
                    .collect();
            }
            WorkItem {
                id: item.id,
                key: format!("{}-{}", identifier, item.sequence_id),
                sequence_id: item.sequence_id,
                title: item.name,
                state_id,
                state,
                priority: Priority::from_plane(item.priority.as_deref()),
                labels: label_names,
                label_ids,
                due: item.target_date,
                created_at: parse_dt(item.created_at.as_deref()),
                updated_at: parse_dt(item.updated_at.as_deref()),
                completed_at: item.completed_at,
                description: html_to_text_multiline(item.description_html.as_deref().unwrap_or("")),
                actions: Vec::new(),
            }
        })
        .collect::<Vec<_>>();
    Project {
        id: api_project.id,
        name: api_project.name,
        identifier,
        states,
        labels,
        items,
        loaded_at: Instant::now(),
    }
}

fn project_identifier_from_name(name: &str) -> String {
    let acronym = name
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|part| part.chars().find(|ch| ch.is_ascii_alphanumeric()))
        .map(|ch| ch.to_ascii_uppercase())
        .collect::<String>();
    let candidate = if acronym.len() >= 2 {
        acronym
    } else {
        name.to_owned()
    };
    let normalized = normalize_project_identifier(&candidate);
    if normalized.is_empty() {
        "PROJ".to_owned()
    } else {
        normalized
    }
}

fn normalize_project_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_uppercase())
        .take(8)
        .collect()
}

fn parse_new_item_tokens(
    input: &str,
    labels: &[Label],
) -> (String, Priority, Vec<String>, Vec<String>) {
    let mut title_words = Vec::new();
    let mut priority = Priority::None;
    let mut label_ids = Vec::new();
    let mut unknown_labels = Vec::new();
    for word in input.split_whitespace() {
        if let Some(tag) = word.strip_prefix('!') {
            let parsed = match tag.to_lowercase().as_str() {
                "u" | "urgent" => Some(Priority::Urgent),
                "h" | "high" => Some(Priority::High),
                "m" | "medium" => Some(Priority::Medium),
                "l" | "low" => Some(Priority::Low),
                _ => None,
            };
            if let Some(parsed) = parsed {
                priority = parsed;
                continue;
            }
        }
        if let Some(name) = word.strip_prefix('#') {
            if !name.is_empty() {
                let query = name.to_lowercase();
                let exact = labels
                    .iter()
                    .find(|label| label.name.to_lowercase() == query);
                let found = exact.or_else(|| {
                    let matches = labels
                        .iter()
                        .filter(|label| label.name.to_lowercase().starts_with(&query))
                        .collect::<Vec<_>>();
                    if matches.len() == 1 {
                        Some(matches[0])
                    } else {
                        None
                    }
                });
                match found {
                    Some(label) => {
                        if !label_ids.contains(&label.id) {
                            label_ids.push(label.id.clone());
                        }
                    }
                    None => unknown_labels.push(name.to_owned()),
                }
                continue;
            }
        }
        title_words.push(word);
    }
    (title_words.join(" "), priority, label_ids, unknown_labels)
}

fn edit_text_in_editor(item_key: &str, initial: &str) -> Result<Option<String>> {
    let path = std::env::temp_dir().join(format!(
        "plane-tui-desc-{}-{item_key}.md",
        std::process::id()
    ));
    fs::write(&path, initial).with_context(|| format!("write {}", path.display()))?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned());
    let mut parts = editor.split_whitespace();
    let bin = parts.next().unwrap_or("vi").to_owned();
    let args = parts.map(str::to_owned).collect::<Vec<_>>();

    disable_raw_mode()?;
    execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )?;
    let status = Command::new(&bin).args(&args).arg(&path).status();
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
    enable_raw_mode()?;

    let status = status.with_context(|| format!("launch editor {bin} (set $EDITOR)"))?;
    if !status.success() {
        let _ = fs::remove_file(&path);
        bail!("{bin} exited with {status}");
    }
    let edited = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let _ = fs::remove_file(&path);
    let edited = edited.trim_end().to_owned();
    if edited == initial.trim_end() {
        return Ok(None);
    }
    Ok(Some(edited))
}

fn text_to_description_html(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| format!("<p>{}</p>", escape_html(paragraph).replace('\n', "<br/>")))
        .collect::<Vec<_>>()
        .join("")
}

fn spawn_agent(config: &Config, out_file: &std::path::Path) -> Result<std::process::Child> {
    let bin = config.agent_bin();
    let mut command = Command::new(bin);
    match config.agent_backend {
        AgentBackend::Codex => {
            command
                .arg("exec")
                .arg("--skip-git-repo-check")
                .arg("--sandbox")
                .arg("read-only")
                .arg("--ephemeral")
                .arg("--color")
                .arg("never")
                .arg("--output-last-message")
                .arg(out_file)
                .arg("-")
                .stdout(Stdio::null());
        }
        AgentBackend::Claude => {
            command
                .arg("--print")
                .arg("--model")
                .arg(&config.claude_model)
                .arg("--effort")
                .arg(&config.claude_effort)
                .arg("--disallowedTools")
                .arg("Edit,Write,NotebookEdit")
                .stdout(Stdio::piped());
        }
    }
    command.stdin(Stdio::piped()).stderr(Stdio::piped());
    if let Some(repo_dir) = config.repo_dir.as_deref() {
        command.current_dir(repo_dir);
    }
    command.spawn().with_context(|| {
        format!("launch {bin} (install it, or pick a backend with :backend codex|claude)")
    })
}

fn complete_agent(
    mut child: std::process::Child,
    backend: AgentBackend,
    bin: &str,
    out_file: &std::path::Path,
    prompt: &str,
) -> Result<String> {
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        bail!("{bin} stdin unavailable");
    };
    stdin
        .write_all(prompt.as_bytes())
        .with_context(|| format!("write prompt to {bin} stdin"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .with_context(|| format!("wait for {bin}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no stderr")
            .to_owned();
        bail!("{bin} failed ({}): {tail}", output.status);
    }
    let text = match backend {
        AgentBackend::Codex => {
            let text = fs::read_to_string(out_file)
                .with_context(|| format!("read {bin} output {}", out_file.display()))?;
            let _ = fs::remove_file(out_file);
            text
        }
        AgentBackend::Claude => String::from_utf8_lossy(&output.stdout).into_owned(),
    };
    if text.trim().is_empty() {
        bail!("{bin} returned an empty prompt");
    }
    Ok(text.trim().to_owned())
}

fn save_prompt(item_key: &str, prompt: &str) -> Result<String> {
    let dir = prompt_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}-agent-prompt.md", item_key.to_lowercase()));
    fs::write(&path, prompt).with_context(|| format!("write {}", path.display()))?;
    Ok(path.display().to_string())
}

fn prompt_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("PLANE_TUI_PROMPT_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(plane_tui_data_dir()?.join("prompts"))
}

fn plane_tui_data_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("plane-tui"));
    }
    Ok(std::env::current_dir()?.join(".plane-tui-data"))
}

#[derive(Debug, Default)]
struct AgentPrefs {
    backend: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

fn agent_prefs_path() -> Result<PathBuf> {
    Ok(plane_tui_data_dir()?.join("agent-backend.tsv"))
}

fn saved_agent_prefs() -> Result<AgentPrefs> {
    let path = agent_prefs_path()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(AgentPrefs::default()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let mut fields = text.trim().split('\t');
    let field = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(AgentPrefs {
        backend: field(fields.next()),
        model: field(fields.next()),
        effort: field(fields.next()),
    })
}

fn save_agent_prefs(backend: AgentBackend, model: &str, effort: &str) -> Result<()> {
    let path = agent_prefs_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(&path, format!("{}\t{model}\t{effort}\n", backend.name()))
        .with_context(|| format!("write {}", path.display()))
}

fn remembered_projects_path() -> Result<PathBuf> {
    Ok(plane_tui_data_dir()?.join("projects.tsv"))
}

fn remembered_projects(workspace: &str) -> Result<Vec<String>> {
    let path = remembered_projects_path()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| {
            let (saved_workspace, identifier) = line.split_once('\t')?;
            (saved_workspace == workspace)
                .then(|| identifier.trim().to_lowercase())
                .filter(|identifier| !identifier.is_empty())
        })
        .collect())
}

fn remember_project(workspace: &str, identifier: &str) -> Result<()> {
    let identifier = identifier.trim().to_lowercase();
    if identifier.is_empty() {
        return Ok(());
    }
    let path = remembered_projects_path()?;
    let mut rows = fs::read_to_string(&path)
        .map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
        .or_else(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                Ok(Vec::new())
            } else {
                Err(err).with_context(|| format!("read {}", path.display()))
            }
        })?;
    let row = format!("{workspace}\t{identifier}");
    if !rows.iter().any(|existing| existing == &row) {
        rows.push(row);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&path, format!("{}\n", rows.join("\n")))
        .with_context(|| format!("write {}", path.display()))
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let tool_copied = ["pbcopy", "wl-copy", "xclip"].iter().any(|bin| {
        let mut command = Command::new(bin);
        if *bin == "xclip" {
            command.args(["-selection", "clipboard"]);
        }
        let Ok(mut child) = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.wait();
            return false;
        };
        let written = stdin.write_all(text.as_bytes()).is_ok();
        drop(stdin);
        written && child.wait().is_ok_and(|status| status.success())
    });

    // OSC 52 asks the terminal itself to set the clipboard (kitty allows writes
    // by default), so it also works over SSH where no local tool can.
    let osc = format!("\x1b]52;c;{}\x1b\\", base64_encode(text.as_bytes()));
    let mut stdout = io::stdout();
    let osc_emitted = stdout
        .write_all(osc.as_bytes())
        .and_then(|_| stdout.flush())
        .is_ok();

    if tool_copied || osc_emitted {
        Ok(())
    } else {
        bail!("no clipboard path worked (pbcopy/wl-copy/xclip and OSC 52 all failed)")
    }
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn string_array_field(value: &Value, field: &str) -> Option<Vec<String>> {
    value.get(field)?.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn distribute_width(total: u16, count: usize, min_width: u16) -> Vec<u16> {
    if count == 0 {
        return Vec::new();
    }
    let count_u16 = count as u16;
    let base = if total / count_u16 >= min_width {
        total / count_u16
    } else {
        max(1, total / count_u16)
    };
    let mut widths = vec![base; count];
    let mut remainder = total.saturating_sub(base.saturating_mul(count_u16));
    let mut index = 0;
    while remainder > 0 {
        widths[index] = widths[index].saturating_add(1);
        remainder -= 1;
        index = (index + 1) % count;
    }
    widths
}

fn parse_dt(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn time_ago(dt: DateTime<Utc>) -> String {
    let delta = Utc::now().signed_duration_since(dt);
    if delta.num_days() <= 0 {
        "today".to_owned()
    } else if delta.num_days() == 1 {
        "1d ago".to_owned()
    } else if delta.num_days() < 32 {
        format!("{}d ago", delta.num_days())
    } else if delta.num_days() < 370 {
        format!("{}mo ago", delta.num_days() / 30)
    } else {
        format!("{}y ago", delta.num_days() / 365)
    }
}

fn html_to_text_multiline(html: &str) -> String {
    let normalized = html
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</p>", "\n\n")
        .replace("</div>", "\n")
        .replace("</li>", "\n")
        .replace("</pre>", "\n")
        .replace("</h1>", "\n")
        .replace("</h2>", "\n")
        .replace("</h3>", "\n");
    let mut text = String::new();
    let mut in_tag = false;
    for ch in normalized.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'");
    let mut out: Vec<String> = Vec::new();
    let mut blanks = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 || out.is_empty() {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push(line.to_owned());
    }
    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn looks_like_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn parse_hex_color(hex: &str) -> Color {
    let clean = hex.trim_start_matches('#');
    if clean.len() != 6 {
        return Color::Grey;
    }
    let Ok(value) = u32::from_str_radix(clean, 16) else {
        return Color::Grey;
    };
    Color::Rgb {
        r: ((value >> 16) & 0xff) as u8,
        g: ((value >> 8) & 0xff) as u8,
        b: (value & 0xff) as u8,
    }
}

fn default_label_color(index: usize) -> &'static str {
    const COLORS: &[&str] = &[
        "#8b5cf6", "#22c55e", "#ef4444", "#f59e0b", "#38bdf8", "#ec4899", "#14b8a6",
    ];
    COLORS[index % COLORS.len()]
}

fn color_marker(color: Color) -> &'static str {
    match color {
        Color::Rgb { r, g, b } if r >= g && r >= b => "■",
        Color::Rgb { r, g, b } if g >= r && g >= b => "▪",
        Color::Rgb { .. } => "◆",
        _ => "▪",
    }
}

fn list_due(value: Option<&str>) -> (String, Color) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return ("-".to_owned(), DIMMER);
    };
    let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") else {
        return (value.to_owned(), TEXT);
    };
    let days = date
        .signed_duration_since(Local::now().date_naive())
        .num_days();
    let text = match days {
        0 => "today".to_owned(),
        1 => "tom".to_owned(),
        _ => value.get(5..).unwrap_or(value).to_owned(),
    };
    let color = if days <= 3 { RED } else { TEXT };
    (text, color)
}

fn truncate(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width + 1 > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

fn wrap_line(value: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        let extra = if current.is_empty() { 0 } else { 1 };
        if current.width() + word.width() + extra > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn scroll_offset(value: usize, delta: isize) -> usize {
    if delta >= 0 {
        value.saturating_add(delta as usize)
    } else {
        value.saturating_sub((-delta) as usize)
    }
}

fn draw_text(out: &mut io::Stdout, x: &mut u16, y: u16, text: &str, fg: Color) -> Result<()> {
    queue!(
        out,
        MoveTo(*x, y),
        SetForegroundColor(fg),
        SetBackgroundColor(BG),
        Print(text),
        ResetColor
    )?;
    *x = x.saturating_add(text.width() as u16);
    Ok(())
}

fn draw_link_field(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    url: &str,
    value_color: Color,
) -> Result<()> {
    draw_cell(out, x, y, width, "", DIM, None, false)?;
    let mut cursor = x;
    draw_span(
        out,
        &mut cursor,
        y,
        &format!("{:<9}", "url"),
        DIMMER,
        Some(BG),
        false,
    )?;
    let remaining = width.saturating_sub(cursor.saturating_sub(x));
    if remaining > 0 {
        let display = truncate(url, remaining as usize);
        // OSC 8 hyperlink: kitty and friends make the visible text clickable.
        queue!(
            out,
            MoveTo(cursor, y),
            SetForegroundColor(value_color),
            SetBackgroundColor(BG),
            Print(format!("\x1b]8;;{url}\x1b\\{display}\x1b]8;;\x1b\\")),
            ResetColor
        )?;
    }
    Ok(())
}

fn draw_field_line(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    name: &str,
    value: &str,
    value_color: Color,
) -> Result<()> {
    draw_cell(out, x, y, width, "", DIM, None, false)?;
    let mut cursor = x;
    draw_span(
        out,
        &mut cursor,
        y,
        &format!("{name:<9}"),
        DIMMER,
        Some(BG),
        false,
    )?;
    let used = cursor.saturating_sub(x);
    let remaining = width.saturating_sub(used);
    if remaining > 0 {
        draw_span(
            out,
            &mut cursor,
            y,
            &truncate(value, remaining as usize),
            value_color,
            Some(BG),
            false,
        )?;
    }
    Ok(())
}

fn draw_label_field(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    project: &Project,
    item: &WorkItem,
) -> Result<()> {
    draw_cell(out, x, y, width, "", DIM, None, false)?;
    let mut cursor = x;
    draw_span(out, &mut cursor, y, "labels   ", DIMMER, Some(BG), false)?;
    if item.label_ids.is_empty() {
        draw_span(
            out,
            &mut cursor,
            y,
            "none · t to add",
            DIMMER,
            Some(BG),
            false,
        )?;
        return Ok(());
    }

    let mut rendered = 0;
    for label_id in &item.label_ids {
        let Some(label) = project.labels.iter().find(|label| &label.id == label_id) else {
            continue;
        };
        if cursor.saturating_sub(x) >= width.saturating_sub(2) {
            break;
        }
        if rendered > 0 {
            draw_span(out, &mut cursor, y, " ", DIM, Some(BG), false)?;
        }
        let remaining = width.saturating_sub(cursor.saturating_sub(x));
        let text = format!("{}{}", color_marker(label.color), label.name);
        draw_span(
            out,
            &mut cursor,
            y,
            &truncate(&text, remaining as usize),
            label.color,
            Some(BG),
            false,
        )?;
        rendered += 1;
    }
    if rendered == 0 {
        let fallback = if item.labels.is_empty() {
            "none".to_owned()
        } else {
            item.labels.join(" ")
        };
        let remaining = width.saturating_sub(cursor.saturating_sub(x));
        draw_span(
            out,
            &mut cursor,
            y,
            &truncate(&fallback, remaining as usize),
            TEXT,
            Some(BG),
            false,
        )?;
    }
    Ok(())
}

fn draw_outer_frame(out: &mut io::Stdout, frame: LayoutFrame) -> Result<()> {
    if frame.x == 0 || frame.y == 0 || frame.width < 2 || frame.height < 2 {
        return Ok(());
    }
    let left = frame.x - 1;
    let top = frame.y - 1;
    let width = frame.width + 2;
    let height = frame.height + 2;
    queue!(
        out,
        MoveTo(left, top),
        SetForegroundColor(LINE),
        SetBackgroundColor(BG),
        Print("┌"),
        Print("─".repeat(width.saturating_sub(2) as usize)),
        Print("┐")
    )?;
    for row in 1..height.saturating_sub(1) {
        queue!(
            out,
            MoveTo(left, top + row),
            SetForegroundColor(LINE),
            SetBackgroundColor(BG),
            Print("│"),
            MoveTo(left + width.saturating_sub(1), top + row),
            Print("│")
        )?;
    }
    queue!(
        out,
        MoveTo(left, top + height.saturating_sub(1)),
        Print("└"),
        Print("─".repeat(width.saturating_sub(2) as usize)),
        Print("┘"),
        ResetColor
    )?;
    Ok(())
}

fn clear_area(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    bg: Option<Color>,
) -> Result<()> {
    let blank = " ".repeat(width as usize);
    for row in 0..height {
        queue!(out, MoveTo(x, y + row), ResetColor)?;
        if let Some(bg) = bg {
            queue!(out, SetBackgroundColor(bg))?;
        }
        queue!(out, Print(&blank), ResetColor)?;
    }
    Ok(())
}

fn draw_span(
    out: &mut io::Stdout,
    x: &mut u16,
    y: u16,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    bold: bool,
) -> Result<()> {
    queue!(out, MoveTo(*x, y), SetForegroundColor(fg))?;
    if let Some(bg) = bg {
        queue!(out, SetBackgroundColor(bg))?;
    }
    if bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    queue!(out, Print(text), ResetColor, SetAttribute(Attribute::Reset))?;
    *x = x.saturating_add(text.width() as u16);
    Ok(())
}

fn draw_span_clipped(
    out: &mut io::Stdout,
    x: &mut u16,
    end: u16,
    y: u16,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    bold: bool,
) -> Result<()> {
    let remaining = end.saturating_sub(*x) as usize;
    if remaining == 0 {
        return Ok(());
    }
    draw_span(out, x, y, &truncate(text, remaining), fg, bg, bold)
}

fn draw_cell(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    bold: bool,
) -> Result<()> {
    let content = truncate(text, width as usize);
    let pad = width as usize - content.width().min(width as usize);
    queue!(out, MoveTo(x, y), SetForegroundColor(fg))?;
    queue!(out, SetBackgroundColor(bg.unwrap_or(BG)))?;
    if bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    queue!(
        out,
        Print(content),
        Print(" ".repeat(pad)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn draw_cell_right(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    bold: bool,
) -> Result<()> {
    let content = truncate(text, width as usize);
    let pad = width as usize - content.width().min(width as usize);
    queue!(out, MoveTo(x, y), SetForegroundColor(fg))?;
    queue!(out, SetBackgroundColor(bg.unwrap_or(BG)))?;
    if bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    queue!(
        out,
        Print(" ".repeat(pad)),
        Print(content),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn next_list_x(x: u16, width: u16, end: u16) -> u16 {
    x.saturating_add(width).saturating_add(LIST_GAP).min(end)
}

fn draw_list_cell(
    out: &mut io::Stdout,
    x: u16,
    end: u16,
    y: u16,
    width: u16,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    bold: bool,
) -> Result<u16> {
    let effective_width = width.min(end.saturating_sub(x));
    if effective_width > 0 {
        draw_cell(out, x, y, effective_width, text, fg, bg, bold)?;
    }
    Ok(next_list_x(x, width, end))
}

fn draw_list_cell_right(
    out: &mut io::Stdout,
    x: u16,
    end: u16,
    y: u16,
    width: u16,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    bold: bool,
) -> Result<u16> {
    let effective_width = width.min(end.saturating_sub(x));
    if effective_width > 0 {
        draw_cell_right(out, x, y, effective_width, text, fg, bg, bold)?;
    }
    Ok(next_list_x(x, width, end))
}

fn draw_card_border(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    color: Color,
    bg: Option<Color>,
) -> Result<()> {
    if width < 2 {
        return Ok(());
    }
    let bg = bg.unwrap_or(BG);
    queue!(
        out,
        MoveTo(x, y),
        SetForegroundColor(color),
        SetBackgroundColor(bg),
        Print("┌"),
        Print("─".repeat(width.saturating_sub(2) as usize)),
        Print("┐")
    )?;
    for row in 1..CARD_HEIGHT.saturating_sub(1) {
        queue!(
            out,
            MoveTo(x, y + row),
            SetForegroundColor(color),
            SetBackgroundColor(bg),
            Print("│"),
            MoveTo(x + width.saturating_sub(1), y + row),
            Print("│")
        )?;
    }
    queue!(
        out,
        MoveTo(x, y + CARD_HEIGHT.saturating_sub(1)),
        SetForegroundColor(color),
        SetBackgroundColor(bg),
        Print("└"),
        Print("─".repeat(width.saturating_sub(2) as usize)),
        Print("┘")
    )?;
    queue!(out, ResetColor)?;
    Ok(())
}

fn draw_help_panel(out: &mut io::Stdout, width: u16, height: u16) -> Result<()> {
    let box_width = min(width.saturating_sub(8), 112);
    let box_height = min(height.saturating_sub(6), 24);
    if box_width < 48 || box_height < 10 {
        return draw_overlay(
            out,
            width,
            height,
            &[
                " keys ",
                "j/k move · h/l columns · D show done · e edit · s state · p priority · t labels",
                "m mark · T triage · / search · : command (:new, :project) · q close",
            ],
        );
    }

    let x = width.saturating_sub(box_width) / 2;
    let y = height.saturating_sub(box_height) / 2;
    draw_modal_shell(out, x, y, box_width, box_height, "keys · shortcuts")?;

    let left = x + 3;
    let value_x = x + 22;
    let mut row = y + 3;
    draw_help_section(out, left, row, "navigate")?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "j / k",
        "move down / up within a column or list",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "h / l",
        "move between board columns",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "gg / G",
        "jump to first / last item",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "enter",
        "item detail · description + comments",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "tab / S-tab / 1 2 3",
        "next · previous · direct project switch",
    )?;
    row += 2;

    draw_help_section(out, left, row, "act on item or marks")?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "s  p  t",
        "state · priority · labels",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "e",
        "edit title · description · due date",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "a / A",
        "agent prompt via claude/codex (:backend) · A also posts it as a comment",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "m / I / U",
        "mark · invert marks · clear marks",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "o / n / R",
        "open browser · new item · refresh",
    )?;
    row += 2;

    draw_help_section(out, left, row, "modes")?;
    row += 1;
    draw_shortcut_row(out, left, value_x, row, "D", "toggle Done board column")?;
    row += 1;
    draw_shortcut_row(out, left, value_x, row, "T", "triage sweep")?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "/  :  f  S",
        "search · command (:new, :project, :backend) · filter · sort",
    )?;
    row += 1;
    draw_shortcut_row(out, left, value_x, row, "? / q / esc", "close this panel")?;

    Ok(())
}

fn draw_modal_shell(
    out: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
) -> Result<()> {
    for row in 0..height {
        draw_cell(out, x, y + row, width, "", TEXT, Some(BG), false)?;
    }
    draw_cell(
        out,
        x,
        y,
        width,
        &format!(" {title}"),
        Color::Black,
        Some(PAPER),
        true,
    )?;
    queue!(
        out,
        MoveTo(x, y + 1),
        SetForegroundColor(PAPER),
        SetBackgroundColor(BG),
        Print("│"),
        MoveTo(x + width.saturating_sub(1), y + 1),
        Print("│")
    )?;
    for row in 2..height.saturating_sub(2) {
        queue!(
            out,
            MoveTo(x, y + row),
            SetForegroundColor(PAPER),
            SetBackgroundColor(BG),
            Print("│"),
            MoveTo(x + width.saturating_sub(1), y + row),
            Print("│")
        )?;
    }
    draw_cell(
        out,
        x,
        y + height.saturating_sub(2),
        width,
        "",
        DIM,
        Some(BG),
        false,
    )?;
    if width > 4 {
        draw_cell(
            out,
            x + 1,
            y + height.saturating_sub(2),
            width.saturating_sub(2),
            &"━".repeat(width.saturating_sub(4) as usize),
            Color::Rgb {
                r: 190,
                g: 190,
                b: 190,
            },
            Some(PAPER),
            false,
        )?;
    }
    draw_cell(
        out,
        x,
        y + height.saturating_sub(1),
        width,
        "",
        PAPER,
        Some(BG),
        false,
    )?;
    queue!(out, ResetColor)?;
    Ok(())
}

fn draw_help_section(out: &mut io::Stdout, x: u16, y: u16, title: &str) -> Result<()> {
    draw_cell(out, x, y, 28, title, DIM, Some(BG), false)
}

fn draw_shortcut_row(
    out: &mut io::Stdout,
    x: u16,
    value_x: u16,
    y: u16,
    keys: &str,
    description: &str,
) -> Result<()> {
    let mut cursor = x;
    draw_span(out, &mut cursor, y, keys, ACCENT, Some(BG), true)?;
    cursor = value_x;
    draw_span(out, &mut cursor, y, description, DIM, Some(BG), false)?;
    Ok(())
}

fn draw_overlay(out: &mut io::Stdout, width: u16, height: u16, lines: &[&str]) -> Result<()> {
    let box_width = min(width.saturating_sub(4), 88);
    let box_height = min(height.saturating_sub(4), lines.len() as u16 + 2);
    let x = width.saturating_sub(box_width) / 2;
    let y = height.saturating_sub(box_height) / 2;
    for row in 0..box_height {
        draw_cell(
            out,
            x,
            y + row,
            box_width,
            "",
            Color::White,
            Some(Color::Black),
            false,
        )?;
    }
    queue!(
        out,
        MoveTo(x, y),
        SetForegroundColor(Color::White),
        SetBackgroundColor(Color::Black),
        Print("┌"),
        Print("─".repeat(box_width.saturating_sub(2) as usize)),
        Print("┐")
    )?;
    for row in 1..box_height.saturating_sub(1) {
        queue!(
            out,
            MoveTo(x, y + row),
            Print("│"),
            MoveTo(x + box_width.saturating_sub(1), y + row),
            Print("│")
        )?;
    }
    queue!(
        out,
        MoveTo(x, y + box_height.saturating_sub(1)),
        Print("└"),
        Print("─".repeat(box_width.saturating_sub(2) as usize)),
        Print("┘"),
        ResetColor
    )?;
    for (index, line) in lines
        .iter()
        .take(box_height.saturating_sub(2) as usize)
        .enumerate()
    {
        draw_cell(
            out,
            x + 2,
            y + 1 + index as u16,
            box_width.saturating_sub(4),
            line,
            if index == 0 {
                Color::Black
            } else {
                Color::Grey
            },
            if index == 0 {
                Some(Color::White)
            } else {
                Some(Color::Black)
            },
            index == 0,
        )?;
    }
    Ok(())
}
