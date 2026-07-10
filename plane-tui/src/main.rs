mod jobs;
use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
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
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
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
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

/// Concurrent agent cap (PLANE_TUI_AGENT_WIP, default 3); excess jobs queue.
fn agent_wip() -> usize {
    env_u64("PLANE_TUI_AGENT_WIP", 3).max(1) as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoSource {
    Default,      // --repo-dir: always available, can't be removed here
    Env,          // PLANE_TUI_REPOS: managed outside the TUI
    Saved,        // repos.tsv: the wizard's own entries
    Unregistered, // discovered but not yet a dispatch target
}

#[derive(Debug, Clone)]
struct RepoPick {
    name: String,
    path: PathBuf,
    kind: &'static str, // "repo" | "submodule"
    source: RepoSource,
    /// Default/env entry the user hid via the wizard ("!" line in repos.tsv).
    hidden: bool,
}

#[derive(Debug, Clone)]
struct SkillPick {
    name: String,
    description: String,
    origin: &'static str, // "repo" | "claude" | "codex"
    selected: bool,
}

/// Parse a SKILL.md's frontmatter for name + first description line.
fn skill_meta(dir: &Path) -> Option<(String, String)> {
    let raw = fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let mut name = dir.file_name()?.to_string_lossy().to_string();
    let mut description = String::new();
    let mut in_frontmatter = false;
    for line in raw.lines().take(40) {
        let trimmed = line.trim();
        if trimmed == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if !in_frontmatter {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let value = value.trim();
            if !value.is_empty() {
                name = value.to_owned();
            }
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            description = value.trim().to_owned();
        }
    }
    Some((name, description))
}

fn scan_skill_root(
    root: &Path,
    origin: &'static str,
    selected: &[String],
    picks: &mut Vec<SkillPick>,
    seen: &mut BTreeSet<String>,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Some((name, description)) = skill_meta(&dir) else {
            continue;
        };
        if !seen.insert(name.to_lowercase()) {
            continue;
        }
        let selected = selected.iter().any(|have| have.eq_ignore_ascii_case(&name));
        picks.push(SkillPick {
            name,
            description,
            origin,
            selected,
        });
    }
}

#[cfg(test)]
mod skill_tests {
    use super::*;

    #[test]
    fn skill_meta_parses_frontmatter() {
        let dir = std::env::temp_dir().join(format!("pti-skill-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: seo-audit\ndescription: Run an SEO audit across the site.\nuser-invokable: true\n---\n# body\n",
        )
        .unwrap();
        let (name, description) = skill_meta(&dir).unwrap();
        assert_eq!(name, "seo-audit");
        assert_eq!(description, "Run an SEO audit across the site.");
        // no frontmatter → falls back to the directory name
        fs::write(dir.join("SKILL.md"), "just a body, no frontmatter\n").unwrap();
        let (name, description) = skill_meta(&dir).unwrap();
        assert!(name.starts_with("pti-skill-test-"));
        assert!(description.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn env_repos() -> Vec<(String, PathBuf)> {
    let Ok(raw) = std::env::var("PLANE_TUI_REPOS") else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|pair| {
            let (name, path) = pair.split_once('=')?;
            let name = name.trim().to_owned();
            if name.is_empty() {
                return None;
            }
            Some((name, expand_tilde(path.trim())))
        })
        .collect()
}

fn repos_registry_path() -> Result<PathBuf> {
    Ok(plane_tui_data_dir()?.join("repos.tsv"))
}

/// repos.tsv holds two kinds of lines: "name\tpath" (wizard-added repos) and
/// "!\tpath" (a default/env entry the user hid via the wizard).
fn read_repos_file() -> Vec<(String, PathBuf)> {
    let Ok(path) = repos_registry_path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| {
            let (name, path) = line.split_once('\t')?;
            Some((name.to_owned(), PathBuf::from(path)))
        })
        .collect()
}

fn saved_repos() -> Vec<(String, PathBuf)> {
    read_repos_file()
        .into_iter()
        .filter(|(name, _)| name != "!")
        .collect()
}

fn hidden_repo_paths() -> Vec<PathBuf> {
    read_repos_file()
        .into_iter()
        .filter(|(name, _)| name == "!")
        .map(|(_, path)| path)
        .collect()
}

fn write_repos_file(repos: &[(String, PathBuf)]) -> Result<()> {
    let path = repos_registry_path()?;
    let body = repos
        .iter()
        .map(|(name, path)| format!("{name}\t{}\n", path.display()))
        .collect::<String>();
    fs::write(path, body).context("writing repos.tsv")
}

fn work_folders_path() -> Result<PathBuf> {
    Ok(plane_tui_data_dir()?.join("work-folders.tsv"))
}

/// Folders previously picked for `w` work sessions, most recent first.
fn saved_work_folders() -> Vec<PathBuf> {
    let Ok(path) = work_folders_path() else {
        return Vec::new();
    };
    let Ok(body) = fs::read_to_string(path) else {
        return Vec::new();
    };
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn remember_work_folder(folder: &Path) -> Result<()> {
    let mut folders = saved_work_folders();
    folders.retain(|have| have != folder);
    folders.insert(0, folder.to_path_buf());
    folders.truncate(20);
    let path = work_folders_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = folders
        .iter()
        .map(|folder| format!("{}\n", folder.display()))
        .collect::<String>();
    fs::write(path, body).context("writing work-folders.tsv")
}

fn git_toplevel(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if top.is_empty() {
        None
    } else {
        Some(PathBuf::from(top))
    }
}

/// Repos agents can be dispatched into: --repo-dir first, then the wizard's
/// persisted picks (repos.tsv, managed via :repos), then PLANE_TUI_REPOS.
/// The chosen repo decides everything downstream: worktree, branch, push
/// target, and where `P` opens the PR — dispatch notes never can.
fn repo_registry(config: &Config) -> Vec<(String, PathBuf)> {
    let mut repos: Vec<(String, PathBuf)> = Vec::new();
    let push = |name: String, path: PathBuf, repos: &mut Vec<(String, PathBuf)>| {
        if !repos
            .iter()
            .any(|(have, have_path)| *have == name || *have_path == path)
        {
            repos.push((name, path));
        }
    };
    if let Some(dir) = &config.repo_dir {
        // normalize to the git toplevel: a --repo-dir pointing inside a repo
        // (e.g. mono/apps) would otherwise dispatch to the enclosing repo
        // while wearing the subdirectory's name
        let raw = expand_tilde(dir);
        let path = git_toplevel(&raw).unwrap_or(raw);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_owned());
        push(name, path, &mut repos);
    }
    for (name, path) in saved_repos() {
        push(name, path, &mut repos);
    }
    for (name, path) in env_repos() {
        push(name, path, &mut repos);
    }
    let hidden = hidden_repo_paths();
    repos.retain(|(_, path)| !hidden.contains(path));
    repos
}

/// Whether the repo self-documents its conventions — agent CLIs auto-load
/// these from the worktree, so the prompt must not restate (or contradict)
/// them.
fn repo_has_agent_docs(repo: &Path) -> bool {
    ["CLAUDE.md", "AGENTS.md"]
        .iter()
        .any(|name| repo.join(name).exists())
}

/// The explore stance: the user's design-first workflow made mechanical.
/// Asks for exactly what a reviewer wants before implementation starts —
/// assumptions, unknown unknowns, architecture-changing questions, options —
/// and forbids touching the real code paths on this attempt.
fn explore_stance_section(item_key: &str) -> String {
    format!(
        "\n## stance: explore before implementing\nThis dispatch wants design work, not implementation — do not wire changes into the real code paths yet.\nDeliver a design doc, committed in the worktree (follow the repo's conventions for where docs live; otherwise `docs/design/{key}.md`), covering:\n- your read of the problem and the relevant code\n- the assumptions you're making — each with what changes if it's wrong\n- risks and unknowns the reviewer probably hasn't considered\n- the questions whose answers would change the architecture, each with your recommended answer\n- two or three options, sketched or prototyped (scratch prototypes are welcome in a clearly separated area), with trade-offs and a recommendation\nYour final message is the executive summary: your recommendation, the load-bearing assumptions, and the questions you need answered.\nIf reviewer feedback on a later attempt asks for implementation, this exploration stance no longer applies — implement.\n",
        key = item_key.to_lowercase(),
    )
}

/// The envelope every executor prompt ends with. Deliberately minimal: only
/// the cockpit's own contract (worktree plumbing, the QUESTION: sentinel it
/// parses, the summary that becomes the Plane comment). Everything about
/// *how* to work belongs to the repo's CLAUDE.md/AGENTS.md and the agent's
/// judgment — repeating it here just creates drift.
fn executor_envelope(branch: &str, repo_has_docs: bool) -> String {
    let docs_line = if repo_has_docs {
        "The repo's own CLAUDE.md / AGENTS.md conventions apply as usual — they aren't repeated here.\n"
    } else {
        ""
    };
    format!(
        "\n## ground rules (the cockpit's, not the repo's)\n{docs_line}- You're in a disposable git worktree on branch `{branch}`. Commit your work; don't push or open PRs — a human reviews and lands from here.\n- Approach, scope, and style are your call. If something adjacent looks broken, note it in your summary rather than expanding the task.\n- If you're blocked, the task is ambiguous, or it turns out to be already done: stop and end your final message with a line starting `QUESTION:` — asking is a good outcome here, not a failure.\n\n## when you finish\nEnd with a short reviewer-facing summary — what you found, what changed, how you verified it. It is posted back to the Plane work item.\n"
    )
}

/// PLANE_TUI_LABEL_EXECUTORS="frontend=claude,infra=codex" — per-label default
/// executor, matched case-insensitively against the item's label names.
fn label_executor(labels: &[String]) -> Option<AgentBackend> {
    let raw = std::env::var("PLANE_TUI_LABEL_EXECUTORS").ok()?;
    for pair in raw.split(',') {
        let mut parts = pair.splitn(2, '=');
        let (Some(label), Some(backend)) = (parts.next(), parts.next()) else {
            continue;
        };
        let label = label.trim().to_lowercase();
        if labels.iter().any(|have| have.to_lowercase() == label) {
            return match backend.trim().to_lowercase().as_str() {
                "claude" => Some(AgentBackend::Claude),
                "codex" => Some(AgentBackend::Codex),
                _ => None,
            };
        }
    }
    None
}
const BUSINESS_CONTEXT: &str = include_str!("business_context.md");
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}

/// A named color scheme. Every drawing routine reads its colors from the
/// process-wide active theme via the `theme()` accessor, so switching schemes
/// at runtime only means swapping the active `Theme` and repainting.
#[derive(Clone, Copy)]
struct Theme {
    name: &'static str,
    /// App background (the darkest/base surface).
    bg: Color,
    /// Slightly raised panels and headers.
    bg_raise: Color,
    /// Card / cell fill.
    cell_bg: Color,
    /// Borders and dividers.
    line: Color,
    /// Brightest text — headings, selection foreground.
    paper: Color,
    /// Foreground for text painted on top of a bright/highlight fill (selection,
    /// tabs, active menu rows). Dark on light schemes, light on dark ones.
    ink: Color,
    /// Muted secondary text.
    dim: Color,
    /// Faint tertiary text and hairlines.
    dimmer: Color,
    /// Primary accent — selection, links, active state.
    accent: Color,
    /// Body text (warm).
    text: Color,
    /// Warning / in-progress.
    amber: Color,
    /// Error / urgent.
    red: Color,
    /// Success / done.
    green: Color,
}

/// The original warm-on-indigo dark scheme — the default.
const THEME_MIDNIGHT: Theme = Theme {
    name: "midnight",
    bg: rgb(9, 12, 17),
    bg_raise: rgb(13, 17, 24),
    cell_bg: rgb(15, 19, 27),
    line: rgb(35, 42, 54),
    paper: rgb(207, 194, 165),
    ink: Color::Black,
    dim: rgb(102, 101, 111),
    dimmer: rgb(70, 72, 84),
    accent: rgb(91, 113, 202),
    text: rgb(205, 174, 132),
    amber: rgb(211, 151, 54),
    red: rgb(224, 105, 91),
    green: rgb(101, 203, 142),
};

/// Gruvbox-flavored warm dark scheme.
const THEME_GRUVBOX: Theme = Theme {
    name: "gruvbox",
    bg: rgb(29, 32, 33),
    bg_raise: rgb(40, 40, 40),
    cell_bg: rgb(50, 48, 47),
    line: rgb(80, 73, 69),
    paper: rgb(251, 241, 199),
    ink: Color::Black,
    dim: rgb(168, 153, 132),
    dimmer: rgb(124, 111, 100),
    accent: rgb(131, 165, 152),
    text: rgb(235, 219, 178),
    amber: rgb(250, 189, 47),
    red: rgb(251, 73, 52),
    green: rgb(184, 187, 38),
};

/// Light scheme — dark ink on warm off-white.
const THEME_DAYLIGHT: Theme = Theme {
    name: "daylight",
    bg: rgb(245, 242, 235),
    bg_raise: rgb(237, 233, 223),
    cell_bg: rgb(231, 226, 214),
    line: rgb(208, 199, 182),
    paper: rgb(41, 37, 32),
    ink: rgb(247, 245, 240),
    dim: rgb(112, 104, 92),
    dimmer: rgb(163, 155, 141),
    accent: rgb(45, 90, 185),
    text: rgb(60, 52, 42),
    amber: rgb(171, 116, 20),
    red: rgb(188, 61, 50),
    green: rgb(43, 130, 74),
};

/// All selectable schemes, in cycle order. The first entry is the default.
const THEMES: &[Theme] = &[THEME_MIDNIGHT, THEME_GRUVBOX, THEME_DAYLIGHT];

thread_local! {
    static ACTIVE_THEME: std::cell::Cell<Theme> = std::cell::Cell::new(THEME_MIDNIGHT);
}

/// The active color scheme. Cheap (a `Copy` of ~64 bytes from a thread-local);
/// rendering happens on the main thread, which is where the theme is set.
fn theme() -> Theme {
    ACTIVE_THEME.with(|t| t.get())
}

/// Swap the active color scheme for the current (rendering) thread.
fn set_active_theme(scheme: Theme) {
    ACTIVE_THEME.with(|t| t.set(scheme));
}

/// Look up a scheme by (case-insensitive) name.
fn theme_by_name(name: &str) -> Option<Theme> {
    let name = name.trim();
    THEMES
        .iter()
        .copied()
        .find(|t| t.name.eq_ignore_ascii_case(name))
}

/// The scheme after `name` in cycle order (wraps); falls back to the default.
fn next_theme(name: &str) -> Theme {
    let index = THEMES.iter().position(|t| t.name == name).unwrap_or(0);
    THEMES[(index + 1) % THEMES.len()]
}

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
    /// Color scheme: midnight, gruvbox, or daylight (also cycled live with C / :theme).
    #[arg(long, env = "PLANE_TUI_THEME")]
    theme: Option<String>,
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
    theme_name: String,
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
            theme_name: args
                .theme
                .filter(|name| !name.trim().is_empty())
                .or_else(saved_theme_name)
                .unwrap_or_else(|| THEME_MIDNIGHT.name.to_owned()),
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
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http, config }
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
            Self::Backlog => theme().dim,
            Self::Todo => theme().paper,
            Self::Started => theme().amber,
            Self::Done => theme().green,
            Self::Cancelled => theme().red,
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
            Self::Urgent => theme().red,
            Self::High => theme().amber,
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
            Self::None => theme().dim,
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

/// The fleet groups jobs by what they ask of you, in `fleet_order()` rank
/// order (so the group boundaries are always contiguous).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FleetBucket {
    NeedsYou = 0,
    Running = 1,
    Queued = 2,
    Done = 3,
}

impl FleetBucket {
    fn of(status: jobs::JobStatus) -> Self {
        match status {
            jobs::JobStatus::Question
            | jobs::JobStatus::Review
            | jobs::JobStatus::Failed
            | jobs::JobStatus::Orphaned => FleetBucket::NeedsYou,
            jobs::JobStatus::Running | jobs::JobStatus::Briefing => FleetBucket::Running,
            jobs::JobStatus::Queued => FleetBucket::Queued,
            jobs::JobStatus::Landed | jobs::JobStatus::Discarded => FleetBucket::Done,
        }
    }
    fn title(self) -> &'static str {
        match self {
            FleetBucket::NeedsYou => "NEEDS YOU",
            FleetBucket::Running => "RUNNING",
            FleetBucket::Queued => "QUEUED",
            FleetBucket::Done => "DONE",
        }
    }
    fn color(self) -> Color {
        match self {
            FleetBucket::NeedsYou => theme().amber,
            FleetBucket::Running => theme().green,
            FleetBucket::Queued => theme().accent,
            FleetBucket::Done => theme().dimmer,
        }
    }
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
    Dispatch,
    Feedback,
    Land,
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
    DispatchExtra,
    FeedbackNote,
    WorkFolder,
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

/// A live `w` session as seen on the tmux server — the fleet's WORKBENCH
/// section. tmux is the source of truth: no job.json, no lifecycle.
#[derive(Debug, Clone, PartialEq)]
struct WorkSession {
    session: String,
    item_key: String,
    cwd: PathBuf,
}

// tmux allocates session names dynamically, but keeping them bounded makes
// status lines and attach commands usable. The item key always wins; the title
// gets every remaining byte.
const WORK_SESSION_NAME_MAX_BYTES: usize = 200;
const WORK_SESSION_PREFIX: &str = "pti-work-";

fn work_session_name(item_key: &str, title: &str) -> String {
    let base = format!("{WORK_SESSION_PREFIX}{}", item_key.to_lowercase());
    let mut slug = String::new();
    let mut separator_pending = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if separator_pending && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(ch);
            separator_pending = false;
        } else if !slug.is_empty() {
            separator_pending = true;
        }
    }
    if slug.is_empty() || base.len() + 2 >= WORK_SESSION_NAME_MAX_BYTES {
        return base;
    }

    let available = WORK_SESSION_NAME_MAX_BYTES - base.len() - 2;
    let end = slug
        .char_indices()
        .take_while(|(index, ch)| index + ch.len_utf8() <= available)
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    let slug = slug[..end].trim_end_matches('-');
    if slug.is_empty() {
        base
    } else {
        format!("{base}--{slug}")
    }
}

fn work_session_item_key(session: &str) -> Option<String> {
    let rest = session.strip_prefix(WORK_SESSION_PREFIX)?;
    let key = rest.split_once("--").map_or(rest, |(key, _)| key);
    (!key.is_empty()).then(|| key.to_uppercase())
}

fn work_session_matches_item(session: &str, item_key: &str) -> bool {
    work_session_item_key(session).is_some_and(|key| key.eq_ignore_ascii_case(item_key))
}

/// The `w` flow: pick a folder for the selected item and get a human-driven
/// interactive agent session there, latest generated prompt on the clipboard.
#[derive(Debug)]
struct WorkWizard {
    item_key: String,
    /// (origin tag, folder) — remembered picks first, then dispatch repos.
    /// One virtual "type a path…" row trails the list.
    folders: Vec<(&'static str, PathBuf)>,
    sel: usize,
}

struct CodexJob {
    key: String,
    backend: AgentBackend,
    comment_path: String,
    pid: u32,
    started: Instant,
    rx: mpsc::Receiver<CodexOutcome>,
    /// Some(job id): this run is generating a dispatch brief — on completion
    /// the text becomes that job's prompt.md instead of a prompt overlay.
    for_dispatch: Option<String>,
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
    /// What the terminal currently shows; the diff target for the next frame.
    screen: Screen,
    force_clear: bool,
    agent_jobs: Vec<jobs::JobHandle>,
    jobs_open: bool,
    jobs_sel_id: Option<String>,
    dispatch_item: Option<String>,
    dispatch_backend: AgentBackend,
    dispatch_interactive: bool,
    dispatch_brief: bool,
    dispatch_explore: bool,
    dispatch_repo: usize,
    repo_wizard: Option<Vec<RepoPick>>,
    repo_wizard_sel: usize,
    work_wizard: Option<WorkWizard>,
    /// Item key carried across the "type a path…" input hop of the `w` flow.
    work_item: Option<String>,
    /// Live `w` sessions shown in the fleet's WORKBENCH section.
    work_sessions: Vec<WorkSession>,
    work_sessions_at: Option<Instant>,
    skill_wizard: Option<Vec<SkillPick>>,
    skill_wizard_sel: usize,
    dispatch_skills: Vec<String>,
    feedback_job: Option<String>,
    feedback_backend: Option<AgentBackend>,
    land_job: Option<String>,
    post_results: Vec<mpsc::Receiver<(String, std::result::Result<(), String>)>>,
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
            screen: Screen::default(),
            force_clear: true,
            agent_jobs: plane_tui_data_dir()
                .ok()
                .map(|dir| jobs::scan(&jobs::jobs_root(&dir)))
                .unwrap_or_default(),
            jobs_open: false,
            jobs_sel_id: None,
            dispatch_item: None,
            dispatch_backend: AgentBackend::Codex,
            dispatch_interactive: true,
            dispatch_brief: false,
            dispatch_explore: false,
            dispatch_repo: 0,
            repo_wizard: None,
            repo_wizard_sel: 0,
            work_wizard: None,
            work_item: None,
            work_sessions: Vec::new(),
            work_sessions_at: None,
            skill_wizard: None,
            skill_wizard_sel: 0,
            dispatch_skills: Vec::new(),
            feedback_job: None,
            feedback_backend: None,
            land_job: None,
            post_results: Vec::new(),
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
        // keep the API drawer's memory bounded over long resident sessions
        if self.api_log.len() > 600 {
            let excess = self.api_log.len() - 500;
            self.api_log.drain(..excess);
        }
        let mut redraw = self.poll_codex_job();
        if self.pump_agent_jobs() {
            redraw = true;
        }
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
                if let Some(job_id) = &job.for_dispatch {
                    let job_id = job_id.clone();
                    self.fail_dispatch_brief(&job_id, "brief worker stopped unexpectedly");
                }
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
        if let Some(job_id) = job.for_dispatch.clone() {
            self.finish_dispatch_brief(&job.key, &job_id, outcome);
            return;
        }
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
            if let Some(job_id) = &job.for_dispatch {
                let job_id = job_id.clone();
                self.fail_dispatch_brief(&job_id, "brief generation cancelled");
            }
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

    fn select_item_by_key(&mut self, key: &str) -> bool {
        let Some(index) = self.find_index_by_key(key) else {
            self.clamp_selection();
            return false;
        };
        let state = self.project().items[index].state;
        if !self.matches(&self.project().items[index]) {
            self.search.clear();
            self.filter = FilterMode::All;
        }

        let mut selected = false;
        if let Some(column) = self
            .board_states()
            .iter()
            .position(|candidate| *candidate == state)
        {
            self.column = column;
            let indices = self.filtered_indices_for_state(state);
            if let Some(row) = indices.iter().position(|candidate| *candidate == index) {
                self.row = row;
                selected = true;
            }
        }
        let indices = self.flat_indices();
        if let Some(cursor) = indices.iter().position(|candidate| *candidate == index) {
            self.cursor = cursor;
            selected = true;
        }
        if !selected {
            self.clamp_selection();
        }
        self.force_clear = true;
        selected
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

    // ------------------------------------------------------ agent cockpit

    /// `s` in the dispatch menu: checkbox picker over installed skills
    /// (repo-scoped first, then ~/.claude/skills and ~/.codex/skills).
    /// Picks are hinted in the prompt, so this runs before generation.
    fn open_skill_wizard(&mut self) {
        let selected = self.dispatch_skills.clone();
        let registry = repo_registry(&self.client.config);
        let repo = registry
            .get(self.dispatch_repo)
            .or_else(|| registry.first())
            .map(|(_, path)| path.clone());
        let mut picks = Vec::new();
        let mut seen = BTreeSet::new();
        if let Some(repo) = &repo {
            scan_skill_root(
                &repo.join(".claude/skills"),
                "repo",
                &selected,
                &mut picks,
                &mut seen,
            );
            scan_skill_root(
                &repo.join(".codex/skills"),
                "repo",
                &selected,
                &mut picks,
                &mut seen,
            );
        }
        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            scan_skill_root(
                &home.join(".claude/skills"),
                "claude",
                &selected,
                &mut picks,
                &mut seen,
            );
            scan_skill_root(
                &home.join(".codex/skills"),
                "codex",
                &selected,
                &mut picks,
                &mut seen,
            );
        }
        if picks.is_empty() {
            self.status =
                "no skills found (~/.claude/skills, ~/.codex/skills, <repo>/.claude/skills)"
                    .to_owned();
            return;
        }
        self.skill_wizard = Some(picks);
        self.skill_wizard_sel = 0;
        self.force_clear = true;
    }

    fn handle_skill_wizard_key(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.skill_wizard.as_ref().map(Vec::len).unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.skill_wizard = None;
                // return to the dispatch menu mid-flow
                if self.dispatch_item.is_some() {
                    self.menu = Some(MenuMode::Dispatch);
                    self.status = format!(
                        "{} skill(s) picked — they'll be hinted in the prompt",
                        self.dispatch_skills.len()
                    );
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.skill_wizard_sel = min(self.skill_wizard_sel + 1, len.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.skill_wizard_sel = self.skill_wizard_sel.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let sel = self.skill_wizard_sel;
                if let Some(picks) = &mut self.skill_wizard {
                    if let Some(pick) = picks.get_mut(sel) {
                        pick.selected = !pick.selected;
                        let name = pick.name.clone();
                        let selected = pick.selected;
                        if selected {
                            if !self
                                .dispatch_skills
                                .iter()
                                .any(|have| have.eq_ignore_ascii_case(&name))
                            {
                                self.dispatch_skills.push(name);
                            }
                        } else {
                            self.dispatch_skills
                                .retain(|have| !have.eq_ignore_ascii_case(&name));
                        }
                    }
                }
            }
            _ => {}
        }
        self.force_clear = true;
        Ok(())
    }

    fn draw_skill_wizard(&self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
        let Some(picks) = &self.skill_wizard else {
            return Ok(());
        };
        let box_width = min(width.saturating_sub(6), 104);
        let box_height = min(height.saturating_sub(4), 30);
        let x = width.saturating_sub(box_width) / 2;
        let y = height.saturating_sub(box_height) / 2;
        draw_modal_shell(
            out,
            x,
            y,
            box_width,
            box_height,
            &format!("skills · {} picked", self.dispatch_skills.len()),
        )?;
        let inner_x = x + 2;
        let inner_width = box_width.saturating_sub(4);
        let mut row = y + 2;
        let visible = box_height.saturating_sub(5) as usize;
        let start = self
            .skill_wizard_sel
            .saturating_sub(visible.saturating_sub(1));
        for (position, pick) in picks.iter().enumerate().skip(start).take(visible) {
            let selected = position == self.skill_wizard_sel;
            let mark = if pick.selected { "[✓]" } else { "[ ]" };
            let text = format!(
                " {mark} {:<24} {:<6} {}",
                truncate(&pick.name, 24),
                pick.origin,
                truncate(&pick.description, 58),
            );
            let (fg, bg) = if selected {
                (theme().ink, Some(theme().paper))
            } else if pick.selected {
                (theme().text, Some(theme().bg))
            } else {
                (theme().dim, Some(theme().bg))
            };
            draw_cell(out, inner_x, row, inner_width, &text, fg, bg, selected)?;
            row += 1;
        }
        draw_cell(
            out,
            inner_x,
            y + box_height.saturating_sub(2),
            inner_width,
            "enter/space toggle · j/k move · esc back to dispatch",
            theme().dim,
            Some(theme().bg),
            false,
        )?;
        Ok(())
    }

    /// `:repos` / `R` in the dispatch menu: discover git repos and manage
    /// which ones are dispatch targets.
    fn open_repo_wizard(&mut self) {
        let picks = self.discover_repos();
        if picks.is_empty() {
            self.status =
                "no git repos found — set --repo-dir or PLANE_TUI_PROJECTS_DIR".to_owned();
            return;
        }
        self.repo_wizard = Some(picks);
        self.repo_wizard_sel = 0;
        self.force_clear = true;
    }

    /// Candidates: everything registered, plus sibling git repos under the
    /// projects dir (PLANE_TUI_PROJECTS_DIR, default --repo-dir's parent),
    /// plus initialized submodules of --repo-dir.
    fn discover_repos(&self) -> Vec<RepoPick> {
        let config = &self.client.config;
        let default_path = config.repo_dir.as_deref().map(|dir| {
            let raw = expand_tilde(dir);
            git_toplevel(&raw).unwrap_or(raw)
        });
        let env_pairs = env_repos();
        let saved = saved_repos();
        let hidden = hidden_repo_paths();
        let mut picks: Vec<RepoPick> = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let push = |name: String,
                    path: PathBuf,
                    kind: &'static str,
                    picks: &mut Vec<RepoPick>,
                    seen: &mut BTreeSet<PathBuf>| {
            if !seen.insert(path.clone()) {
                return;
            }
            let source = if default_path.as_ref() == Some(&path) {
                RepoSource::Default
            } else if env_pairs.iter().any(|(_, have)| *have == path) {
                RepoSource::Env
            } else if saved.iter().any(|(_, have)| *have == path) {
                RepoSource::Saved
            } else {
                RepoSource::Unregistered
            };
            let hidden = hidden.contains(&path);
            picks.push(RepoPick {
                name,
                path,
                kind,
                source,
                hidden,
            });
        };
        if let Some(path) = &default_path {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_owned());
            push(name, path.clone(), "repo", &mut picks, &mut seen);
        }
        for (name, path) in &saved {
            push(name.clone(), path.clone(), "repo", &mut picks, &mut seen);
        }
        for (name, path) in &env_pairs {
            push(name.clone(), path.clone(), "repo", &mut picks, &mut seen);
        }
        // initialized submodules of every known repo — work meant for one of
        // these is the case a dispatch note can't reach
        let roots: Vec<PathBuf> = picks
            .iter()
            .filter_map(|pick| git_toplevel(&pick.path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for root in roots {
            let mut submodules = Command::new("git");
            submodules
                .arg("-C")
                .arg(&root)
                .args([
                    "config",
                    "--file",
                    ".gitmodules",
                    "--get-regexp",
                    r"^submodule\..*\.path$",
                ])
                .stdin(Stdio::null());
            if let Ok(output) = submodules.output() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let Some((_, rel)) = line.split_once(' ') else {
                        continue;
                    };
                    let path = root.join(rel.trim());
                    if path.join(".git").exists() {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| rel.trim().to_owned());
                        push(name, path, "submodule", &mut picks, &mut seen);
                    }
                }
            }
        }
        // sibling repos under the projects dir
        let projects_dir = std::env::var("PLANE_TUI_PROJECTS_DIR")
            .map(|raw| expand_tilde(&raw))
            .ok()
            .or_else(|| {
                default_path
                    .as_ref()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
            })
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|home| PathBuf::from(home).join("projects"))
            });
        if let Some(projects_dir) = projects_dir {
            if let Ok(entries) = fs::read_dir(&projects_dir) {
                let mut siblings: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir() && path.join(".git").exists())
                    .collect();
                siblings.sort();
                for path in siblings {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "repo".to_owned());
                    push(name, path, "repo", &mut picks, &mut seen);
                }
            }
        }
        picks
    }

    fn handle_repo_wizard_key(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.repo_wizard.as_ref().map(Vec::len).unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.repo_wizard = None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.repo_wizard_sel = min(self.repo_wizard_sel + 1, len.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.repo_wizard_sel = self.repo_wizard_sel.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let sel = self.repo_wizard_sel;
                let Some(pick) = self
                    .repo_wizard
                    .as_ref()
                    .and_then(|picks| picks.get(sel))
                    .cloned()
                else {
                    return Ok(());
                };
                match pick.source {
                    // default/env entries live elsewhere — the wizard hides
                    // or re-enables them via a "!" marker in repos.tsv
                    RepoSource::Default | RepoSource::Env => {
                        let mut file = read_repos_file();
                        if pick.hidden {
                            file.retain(|(name, path)| !(name == "!" && *path == pick.path));
                            self.status = format!("{} re-enabled as a dispatch target", pick.name);
                        } else {
                            file.push(("!".to_owned(), pick.path.clone()));
                            self.status = format!(
                                "{} hidden — r in the dispatch menu skips it now",
                                pick.name
                            );
                        }
                        let result = write_repos_file(&file);
                        self.soft(result);
                        if let Some(picks) = &mut self.repo_wizard {
                            picks[sel].hidden = !pick.hidden;
                        }
                    }
                    RepoSource::Saved => {
                        let mut file = read_repos_file();
                        file.retain(|(name, path)| name == "!" || *path != pick.path);
                        let result = write_repos_file(&file);
                        self.soft(result);
                        if let Some(picks) = &mut self.repo_wizard {
                            picks[sel].source = RepoSource::Unregistered;
                        }
                        self.status = format!("{} removed from dispatch targets", pick.name);
                    }
                    RepoSource::Unregistered => {
                        let mut file = read_repos_file();
                        let mut name = pick.name.clone();
                        let taken = |name: &str, file: &[(String, PathBuf)]| {
                            file.iter().any(|(have, _)| have != "!" && have == name)
                                || env_repos().iter().any(|(have, _)| have == name)
                        };
                        let mut n = 2;
                        while taken(&name, &file) {
                            name = format!("{}-{n}", pick.name);
                            n += 1;
                        }
                        file.push((name.clone(), pick.path.clone()));
                        let result = write_repos_file(&file);
                        self.soft(result);
                        if let Some(picks) = &mut self.repo_wizard {
                            picks[sel].source = RepoSource::Saved;
                        }
                        self.status = format!("{name} added — r in the dispatch menu selects it");
                    }
                }
            }
            _ => {}
        }
        self.force_clear = true;
        Ok(())
    }

    fn draw_repo_wizard(&self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
        let Some(picks) = &self.repo_wizard else {
            return Ok(());
        };
        let box_width = min(width.saturating_sub(6), 100);
        let box_height = min(height.saturating_sub(4), (picks.len() as u16 + 6).max(10));
        let x = width.saturating_sub(box_width) / 2;
        let y = height.saturating_sub(box_height) / 2;
        draw_modal_shell(out, x, y, box_width, box_height, "repos · dispatch targets")?;
        let inner_x = x + 2;
        let inner_width = box_width.saturating_sub(4);
        let mut row = y + 2;
        let visible = box_height.saturating_sub(5) as usize;
        let start = self
            .repo_wizard_sel
            .saturating_sub(visible.saturating_sub(1));
        for (position, pick) in picks.iter().enumerate().skip(start).take(visible) {
            let selected = position == self.repo_wizard_sel;
            let mark = if pick.source == RepoSource::Unregistered || pick.hidden {
                "[ ]"
            } else {
                "[✓]"
            };
            let origin = match (pick.source, pick.hidden) {
                (RepoSource::Default, true) => "default·off",
                (RepoSource::Default, false) => "--repo-dir",
                (RepoSource::Env, true) => "env·off",
                (RepoSource::Env, false) => "env",
                (RepoSource::Saved, _) => "saved",
                (RepoSource::Unregistered, _) => "",
            };
            let text = format!(
                " {mark} {:<18} {:<9} {:<10} {}",
                truncate(&pick.name, 18),
                pick.kind,
                origin,
                truncate(&pick.path.display().to_string(), 48),
            );
            let (fg, bg) = if selected {
                (theme().ink, Some(theme().paper))
            } else if pick.source == RepoSource::Unregistered {
                (theme().dim, Some(theme().bg))
            } else {
                (theme().text, Some(theme().bg))
            };
            draw_cell(out, inner_x, row, inner_width, &text, fg, bg, selected)?;
            row += 1;
        }
        draw_cell(
            out,
            inner_x,
            y + box_height.saturating_sub(2),
            inner_width,
            "enter/space add·remove · j/k move · esc close · r in dispatch menu cycles these",
            theme().dim,
            Some(theme().bg),
            false,
        )?;
        Ok(())
    }

    /// `d` on the selected item: choose an executor, then an optional note.
    /// `w`: one keystroke from the selected item to a human-driven agent
    /// session — pick a folder (remembered across runs), the item's latest
    /// generated prompt lands on the clipboard, and an interactive
    /// claude/codex opens there in a tmux pane.
    fn open_work_wizard(&mut self) {
        let Some(item) = self.current_item() else {
            self.status = "no item selected".to_owned();
            return;
        };
        let item_key = item.key.clone();
        let mut folders: Vec<(&'static str, PathBuf)> = Vec::new();
        for folder in saved_work_folders() {
            if !folders.iter().any(|(_, have)| *have == folder) {
                folders.push(("recent", folder));
            }
        }
        for (_, path) in repo_registry(&self.client.config) {
            if !folders.iter().any(|(_, have)| *have == path) {
                folders.push(("repo", path));
            }
        }
        self.work_wizard = Some(WorkWizard {
            item_key,
            folders,
            sel: 0,
        });
        self.force_clear = true;
    }

    fn handle_work_wizard_key(&mut self, key: KeyEvent) -> Result<()> {
        // + 1: the trailing "type a path…" row
        let len = self
            .work_wizard
            .as_ref()
            .map(|wizard| wizard.folders.len() + 1)
            .unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.work_wizard = None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(wizard) = &mut self.work_wizard {
                    wizard.sel = min(wizard.sel + 1, len.saturating_sub(1));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(wizard) = &mut self.work_wizard {
                    wizard.sel = wizard.sel.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                let Some(wizard) = self.work_wizard.take() else {
                    return Ok(());
                };
                match wizard.folders.get(wizard.sel) {
                    Some((_, folder)) => {
                        let folder = folder.clone();
                        self.launch_work_session(&wizard.item_key, &folder);
                    }
                    None => {
                        self.work_item = Some(wizard.item_key);
                        self.input_mode = Some(InputMode::WorkFolder);
                        self.input.clear();
                        self.input_cursor = 0;
                    }
                }
            }
            _ => {}
        }
        self.force_clear = true;
        Ok(())
    }

    fn draw_work_wizard(&self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
        let Some(wizard) = &self.work_wizard else {
            return Ok(());
        };
        let rows = wizard.folders.len() + 1;
        let box_width = min(width.saturating_sub(6), 100);
        let box_height = min(height.saturating_sub(4), (rows as u16 + 6).max(10));
        let x = width.saturating_sub(box_width) / 2;
        let y = height.saturating_sub(box_height) / 2;
        draw_modal_shell(
            out,
            x,
            y,
            box_width,
            box_height,
            &format!("work on {} · pick a folder", wizard.item_key),
        )?;
        let inner_x = x + 2;
        let inner_width = box_width.saturating_sub(4);
        let mut row = y + 2;
        let visible = box_height.saturating_sub(5) as usize;
        let start = wizard.sel.saturating_sub(visible.saturating_sub(1));
        for position in start..min(rows, start + visible) {
            let selected = position == wizard.sel;
            let text = match wizard.folders.get(position) {
                Some((origin, path)) => {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    format!(
                        " {:<20} {:<7} {}",
                        truncate(&name, 20),
                        origin,
                        truncate(&path.display().to_string(), 60),
                    )
                }
                None => " type a path…".to_owned(),
            };
            let (fg, bg) = if selected {
                (theme().ink, Some(theme().paper))
            } else {
                (theme().text, Some(theme().bg))
            };
            draw_cell(out, inner_x, row, inner_width, &text, fg, bg, selected)?;
            row += 1;
        }
        draw_cell(
            out,
            inner_x,
            y + box_height.saturating_sub(2),
            inner_width,
            "enter open interactive session here (prompt → clipboard) · j/k move · esc cancel",
            theme().dim,
            Some(theme().bg),
            false,
        )?;
        Ok(())
    }

    /// The freshest generated prompt for an item: the `a`/`A` prompt file and
    /// any dispatched job's prompt.md compete on mtime.
    fn latest_item_prompt(&self, item_key: &str) -> Option<String> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(dir) = prompt_dir() {
            candidates.push(dir.join(format!("{}-agent-prompt.md", item_key.to_lowercase())));
        }
        for handle in &self.agent_jobs {
            if handle.job.item_key == item_key {
                candidates.push(handle.dir.join("prompt.md"));
            }
        }
        candidates
            .into_iter()
            .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
            .max_by_key(|(modified, _)| *modified)
            .and_then(|(_, path)| fs::read_to_string(path).ok())
            .filter(|text| !text.trim().is_empty())
    }

    /// Open (or rejoin) the item's interactive work session in `folder`,
    /// with the latest generated prompt on the clipboard for pasting. Unlike
    /// dispatch jobs this is plain human-driven work: no worktree, no
    /// wrapper script, approvals at the agent's defaults.
    fn launch_work_session(&mut self, item_key: &str, folder: &Path) {
        if !folder.is_dir() {
            self.status = format!("{} is not a directory", folder.display());
            return;
        }
        let _ = remember_work_folder(folder);
        let clip_note = match self.latest_item_prompt(item_key) {
            Some(prompt) => match copy_to_clipboard(&prompt) {
                Ok(()) => "prompt on clipboard",
                Err(_) => "clipboard failed",
            },
            None => "no generated prompt yet (a generates one)",
        };
        let config = &self.client.config;
        let socket = jobs::default_socket();
        let title = self
            .find_index_by_key(item_key)
            .map(|index| self.project().items[index].title.as_str())
            .unwrap_or_default();
        let preferred_session = work_session_name(item_key, title);
        // Rejoin both legacy key-only sessions and title-bearing sessions made
        // before an item was renamed instead of opening a duplicate.
        let session = jobs::list_sessions_with_prefix(&socket, WORK_SESSION_PREFIX)
            .into_iter()
            .map(|(session, _)| session)
            .find(|session| work_session_matches_item(session, item_key))
            .unwrap_or(preferred_session);
        let mut verb = "rejoined";
        if !jobs::session_alive_raw(&socket, &session) {
            verb = "opened";
            // same approvals-off posture as an interactive dispatch, so the
            // agent never stops for a permission dialog in the pane
            let permission_mode = std::env::var("PLANE_TUI_CLAUDE_PERM")
                .unwrap_or_else(|_| "bypassPermissions".to_owned());
            let command = jobs::interactive_agent_argv(
                config.agent_backend.name(),
                config.agent_bin(),
                &config.claude_model,
                &config.claude_effort,
                &permission_mode,
            );
            if let Err(err) = jobs::spawn_raw(&socket, &session, folder, &command, None) {
                self.status = format!("work session failed: {err:#}");
                return;
            }
        }
        let terminal_cmd = std::env::var("PLANE_TUI_TERMINAL_CMD").ok();
        let template = terminal_cmd.as_deref().unwrap_or("kitty -e {cmd}");
        // a rejoined session keeps the cwd it was created with, which may
        // not be the folder just picked — don't claim otherwise
        let place = if verb == "opened" {
            format!(" in {}", folder.display())
        } else {
            String::new()
        };
        match jobs::deep_dive_session(&socket, &session, Some(template)) {
            Ok(jobs::DeepDive::Switched) | Ok(jobs::DeepDive::SpawnedTerminal(_)) => {
                self.status = format!("{verb} {session}{place} · {clip_note}");
            }
            // the prompt owns the clipboard — show the attach command
            // instead of copying it over the prompt like deep dive does
            Ok(jobs::DeepDive::CopyCommand(cmd)) => {
                self.status = format!("{verb} {session} · attach: {cmd} · {clip_note}");
            }
            Err(err) => {
                self.status = format!("{verb} {session}, but couldn't enter it: {err:#}");
            }
        }
        self.refresh_work_sessions();
        self.force_clear = true;
    }

    /// Sync the fleet's WORKBENCH section with the tmux server (one
    /// list-panes round trip). Returns whether the list changed.
    fn refresh_work_sessions(&mut self) -> bool {
        let sessions: Vec<WorkSession> =
            jobs::list_sessions_with_prefix(&jobs::default_socket(), WORK_SESSION_PREFIX)
                .into_iter()
                .filter_map(|(session, cwd)| {
                    Some(WorkSession {
                        item_key: work_session_item_key(&session)?,
                        session,
                        cwd,
                    })
                })
                .collect();
        self.work_sessions_at = Some(Instant::now());
        if sessions != self.work_sessions {
            self.work_sessions = sessions;
            return true;
        }
        false
    }

    /// Re-enter a live work session from the fleet.
    fn enter_work_session(&mut self, session: &str) {
        let socket = jobs::default_socket();
        let terminal_cmd = std::env::var("PLANE_TUI_TERMINAL_CMD").ok();
        let template = terminal_cmd.as_deref().unwrap_or("kitty -e {cmd}");
        match jobs::deep_dive_session(&socket, session, Some(template)) {
            Ok(jobs::DeepDive::Switched) => self.status = format!("→ {session}"),
            Ok(jobs::DeepDive::SpawnedTerminal(cmd)) => self.status = format!("→ {cmd}"),
            Ok(jobs::DeepDive::CopyCommand(cmd)) => {
                let _ = copy_to_clipboard(&cmd);
                self.status = format!("attach command copied · {cmd}");
            }
            Err(err) => self.status = format!("couldn't enter {session}: {err:#}"),
        }
    }

    fn start_dispatch(&mut self) {
        if repo_registry(&self.client.config).is_empty() {
            self.status =
                "dispatch needs --repo-dir / PLANE_TUI_REPO_DIR or PLANE_TUI_REPOS (the repos agents work in)"
                    .to_owned();
            return;
        }
        let Some(item) = self.current_item() else {
            self.status = "no item selected".to_owned();
            return;
        };
        let key = item.key.clone();
        if self
            .agent_jobs
            .iter()
            .any(|handle| handle.job.item_key == key && handle.job.status.is_active())
        {
            self.status = format!("{key} already has an active job — J opens the fleet");
            return;
        }
        self.dispatch_backend = label_executor(&item.labels).unwrap_or(AgentBackend::Codex);
        self.dispatch_interactive = true;
        self.dispatch_explore = false;
        self.dispatch_repo = 0;
        self.dispatch_skills.clear();
        self.dispatch_item = Some(key);
        self.menu = Some(MenuMode::Dispatch);
        self.force_clear = true;
    }

    fn dispatch_job(&mut self) -> Result<()> {
        let Some(item_key) = self.dispatch_item.take() else {
            bail!("no dispatch in progress");
        };
        let extra = self.input.trim().to_owned();
        let Some(index) = self.find_index_by_key(&item_key) else {
            bail!("{item_key} is no longer on the board");
        };
        let item = self.project().items[index].clone();
        let project_id = self.project().id.clone();
        let registry = repo_registry(&self.client.config);
        let (repo_label, repo) = registry
            .get(self.dispatch_repo)
            .or_else(|| registry.first())
            .cloned()
            .context("no dispatch repo configured (--repo-dir or PLANE_TUI_REPOS)")?;
        let slug = item
            .title
            .to_lowercase()
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>();
        let slug = slug
            .split('-')
            .filter(|part| !part.is_empty())
            .take(4)
            .collect::<Vec<_>>()
            .join("-");
        let branch = if slug.is_empty() {
            format!("{}-agent", item_key.to_lowercase())
        } else {
            format!("{}-{slug}", item_key.to_lowercase())
        };
        let worktree_root = std::env::var("PLANE_TUI_WORKTREE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
                PathBuf::from(home).join("projects").join("worktrees")
            });
        let repo_name = repo
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_owned());
        let worktree_base = worktree_root.join(format!("{repo_name}-{}", item_key.to_lowercase()));
        let attempt = 1 + self
            .agent_jobs
            .iter()
            .filter(|handle| handle.job.item_key == item_key)
            .map(|handle| handle.job.attempt)
            .max()
            .unwrap_or(0);
        let created = Utc::now();
        let id = format!(
            "{}-{}",
            created.format("%Y%m%d-%H%M%S"),
            item_key.to_lowercase()
        );
        let dir = jobs::jobs_root(&plane_tui_data_dir()?).join(&id);
        let (worktree, branch, slot_note) =
            jobs::allocate_worktree(&repo, &worktree_base, &branch)?;
        let base_ref = jobs::create_worktree(&repo, &worktree, &branch)?;
        if let Some(note) = &slot_note {
            self.api_log
                .push(ApiLog::new("AGENT", &item_key, note, "ok", 0));
        }
        let (backend, model, effort) = match self.dispatch_backend {
            AgentBackend::Codex => ("codex".to_owned(), "gpt-5.5".to_owned(), String::new()),
            AgentBackend::Claude => (
                "claude".to_owned(),
                self.client.config.claude_model.clone(),
                self.client.config.claude_effort.clone(),
            ),
        };
        let job = jobs::Job {
            id,
            item_key: item_key.clone(),
            item_id: item.id.clone(),
            project_id,
            title: item.title.clone(),
            backend: backend.clone(),
            model,
            effort,
            attempt,
            repo,
            worktree,
            branch: branch.clone(),
            base_ref,
            tmux_socket: jobs::default_socket(),
            tmux_session: jobs::session_name(&item_key, attempt),
            status: jobs::JobStatus::Queued,
            created_at: created.to_rfc3339(),
            started_at: None,
            mode: if self.dispatch_interactive {
                jobs::JobMode::Interactive
            } else {
                jobs::JobMode::Headless
            },
            stance: if self.dispatch_explore {
                jobs::JobStance::Explore
            } else {
                jobs::JobStance::Implement
            },
            skills: self.dispatch_skills.clone(),
        };
        // two-stage briefing: hand the item to the fable-5 brief generator
        // first; the job sits in BRIEFING until the brief becomes prompt.md
        let brief_stage = self.dispatch_brief && self.codex_job.is_none();
        if self.dispatch_brief && !brief_stage {
            self.status = "brief generator busy — dispatching with the envelope prompt".to_owned();
        }
        let mut job = job;
        if brief_stage {
            job.status = jobs::JobStatus::Briefing;
        }
        jobs::save(&dir, &job)?;
        let prompt = self.build_executor_prompt(&item, &extra, &job);
        if !brief_stage {
            fs::write(dir.join("prompt.md"), prompt)?;
        }
        let job_id = job.id.clone();
        let interactive = job.mode == jobs::JobMode::Interactive;
        self.agent_jobs.push(jobs::JobHandle::new(dir, job));
        let index = self.agent_jobs.len() - 1;
        if brief_stage {
            self.generate_dispatch_brief(&item, &extra, job_id);
            self.status = format!(
                "{item_key} briefing with {} — executes when the brief is ready",
                self.client.config.agent_backend.name()
            );
        } else if self.running_agents() < agent_wip() {
            self.spawn_agent_job(index)?;
            if interactive {
                let job = self.agent_jobs[index].job.clone();
                self.deep_dive_job(&job);
            } else {
                self.status = format!(
                    "{item_key} dispatched → {backend} on {branch} in {repo_label} · J fleet"
                );
            }
        } else {
            self.status = format!(
                "{item_key} queued ({}/{} agents busy) — starts when a slot frees",
                self.running_agents(),
                agent_wip()
            );
        }
        if item.state != StateKind::Started && item.state != StateKind::Done {
            let result =
                self.with_single_target(&item_key, |app| app.apply_state(StateKind::Started));
            self.soft(result);
        }
        if let Some(note) = slot_note {
            self.status = format!("{} · {note}", self.status);
        }
        self.force_clear = true;
        Ok(())
    }

    fn running_agents(&self) -> usize {
        self.agent_jobs
            .iter()
            .filter(|handle| handle.job.status == jobs::JobStatus::Running)
            .count()
    }

    fn job_index_by_id(&self, id: &str) -> Option<usize> {
        self.agent_jobs
            .iter()
            .position(|handle| handle.job.id == id)
    }

    /// (Re)start one job's agent: fresh attempt files, fresh timestamps.
    fn spawn_agent_job(&mut self, index: usize) -> Result<()> {
        let permission_mode = std::env::var("PLANE_TUI_CLAUDE_PERM")
            .unwrap_or_else(|_| "bypassPermissions".to_owned());
        let handle = &mut self.agent_jobs[index];
        jobs::reset_attempt_files(&handle.dir);
        handle.job.started_at = Some(Utc::now().to_rfc3339());
        handle.last_activity = Some(Instant::now());
        handle.stalled = false;
        match jobs::spawn(&handle.job, &handle.dir, &permission_mode) {
            Ok(()) => {
                handle.job.status = jobs::JobStatus::Running;
                let _ = jobs::save(&handle.dir, &handle.job);
                Ok(())
            }
            Err(err) => {
                handle.job.status = jobs::JobStatus::Failed;
                handle.tail.push(format!("spawn failed: {err:#}"));
                let _ = jobs::save(&handle.dir, &handle.job);
                Err(err)
            }
        }
    }

    /// `f` on a finished job: thread the note (and previous result) into
    /// prompt.md, bump the attempt, keep the worktree and its commits.
    fn requeue_with_feedback(&mut self) -> Result<()> {
        let Some(job_id) = self.feedback_job.take() else {
            bail!("no feedback in progress");
        };
        let note = self.input.trim().to_owned();
        let note = if note.is_empty() {
            "address the previous result and continue".to_owned()
        } else {
            note
        };
        let Some(index) = self.job_index_by_id(&job_id) else {
            bail!("job no longer tracked");
        };
        let finished_attempt = self.agent_jobs[index].job.attempt;
        jobs::append_feedback(&self.agent_jobs[index].dir, finished_attempt, &note)?;
        let switched = self.feedback_backend.take();
        {
            let handle = &mut self.agent_jobs[index];
            jobs::kill_session(&handle.job);
            handle.job.attempt += 1;
            handle.job.tmux_session = jobs::session_name(&handle.job.item_key, handle.job.attempt);
            handle.diff_stat = None;
            handle.tail.push(format!(
                "── attempt {} · feedback given ──",
                handle.job.attempt
            ));
        }
        if let Some(backend) = switched {
            let (backend, model, effort) = match backend {
                AgentBackend::Codex => ("codex".to_owned(), "gpt-5.5".to_owned(), String::new()),
                AgentBackend::Claude => (
                    "claude".to_owned(),
                    self.client.config.claude_model.clone(),
                    self.client.config.claude_effort.clone(),
                ),
            };
            let handle = &mut self.agent_jobs[index];
            handle.job.backend = backend;
            handle.job.model = model;
            handle.job.effort = effort;
        }
        let key = self.agent_jobs[index].job.item_key.clone();
        if self.running_agents() < agent_wip() {
            self.spawn_agent_job(index)?;
            self.status = format!(
                "{key} requeued with feedback — attempt {} running",
                self.agent_jobs[index].job.attempt
            );
        } else {
            self.agent_jobs[index].job.status = jobs::JobStatus::Queued;
            let _ = jobs::save(&self.agent_jobs[index].dir, &self.agent_jobs[index].job);
            self.status = format!("{key} queued with feedback — starts when a slot frees");
        }
        self.jobs_open = true;
        self.force_clear = true;
        Ok(())
    }

    /// The land menu's verdict: m merge · P push + PR · b push only.
    fn land_selected(&mut self, how: char) {
        let Some(job_id) = self.land_job.take() else {
            return;
        };
        let Some(index) = self.job_index_by_id(&job_id) else {
            return;
        };
        let job = self.agent_jobs[index].job.clone();
        // guard the empty-PR trap: an agent that worked inside a submodule
        // checkout leaves the job branch itself without commits
        if !jobs::branch_has_commits(&job) {
            self.status = format!(
                "{}: no commits on {} — if the agent worked in a submodule, its branch lives inside {} · not landing",
                job.item_key,
                job.branch,
                job.worktree.display()
            );
            self.jobs_open = true;
            self.force_clear = true;
            return;
        }
        let verb = match how {
            'm' => "merging",
            'P' => "pushing + opening PR for",
            _ => "pushing",
        };
        let job_for_thread = job.clone();
        let outcome = self.run_busy(format!("{verb} {}", job.branch), move |_| match how {
            'm' => jobs::land_merge(&job_for_thread).map(|target| format!("merged into {target}")),
            'P' => jobs::land_push(&job_for_thread, true),
            _ => jobs::land_push(&job_for_thread, false),
        });
        match outcome {
            Ok(note) => {
                self.agent_jobs[index].job.status = jobs::JobStatus::Landed;
                let _ = jobs::save(&self.agent_jobs[index].dir, &self.agent_jobs[index].job);
                let item_key = job.item_key.clone();
                let result =
                    self.with_single_target(&item_key, |app| app.apply_state(StateKind::Done));
                self.soft(result);
                self.post_plain_comment(
                    &job,
                    &format!("plane-tui: landed — {note} (branch {})", job.branch),
                );
                let cleanup = if how == 'm' {
                    "branch + worktree removed"
                } else {
                    "worktree kept until the branch merges"
                };
                self.status = format!("{} landed · {note} · {cleanup}", job.item_key);
            }
            Err(err) => {
                self.status = format!("land failed: {err:#} — f can send the agent back");
            }
        }
        self.jobs_open = true;
        self.force_clear = true;
    }

    /// `enter` on a reviewable job: full diff in git's own pager, TUI suspended.
    fn view_diff_in_pager(&mut self, job: &jobs::Job) -> Result<()> {
        disable_raw_mode()?;
        execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        )?;
        let status = Command::new("git")
            .arg("-C")
            .arg(&job.worktree)
            .args(["diff", &format!("{}..HEAD", job.base_ref)])
            .status();
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
        enable_raw_mode()?;
        self.invalidate_screen();
        status.context("running git diff (pager)")?;
        Ok(())
    }

    /// Background comment post with retries — plain-paragraph variant used
    /// for landing notes; the TUI stays the only Plane writer.
    fn post_plain_comment(&mut self, job: &jobs::Job, text: &str) {
        let client = self.client.clone();
        let project_id = job.project_id.clone();
        let item_id = job.item_id.clone();
        let key = job.item_key.clone();
        let body_text = text.to_owned();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let body = json!({
                "comment_html": format!("<p>{}</p>", escape_html(&body_text)),
            });
            let mut outcome = Err("not attempted".to_owned());
            for attempt in 0u32..3 {
                match client.create_comment(&project_id, &item_id, body.clone()) {
                    Ok(_) => {
                        outcome = Ok(());
                        break;
                    }
                    Err(err) => outcome = Err(format!("{err:#}")),
                }
                thread::sleep(Duration::from_secs(2u64 << attempt));
            }
            let _ = tx.send((key, outcome));
        });
        self.post_results.push(rx);
    }

    /// A hint section listing the skills picked at dispatch, enriched with
    /// their descriptions when still discoverable. A nudge, not a mandate —
    /// both CLIs load their skill registries themselves.
    fn skills_prompt_section(&self, skills: &[String]) -> String {
        if skills.is_empty() {
            return String::new();
        }
        let mut catalog: Vec<SkillPick> = Vec::new();
        let mut seen = BTreeSet::new();
        let registry = repo_registry(&self.client.config);
        for (_, repo) in &registry {
            scan_skill_root(
                &repo.join(".claude/skills"),
                "repo",
                &[],
                &mut catalog,
                &mut seen,
            );
            scan_skill_root(
                &repo.join(".codex/skills"),
                "repo",
                &[],
                &mut catalog,
                &mut seen,
            );
        }
        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            scan_skill_root(
                &home.join(".claude/skills"),
                "claude",
                &[],
                &mut catalog,
                &mut seen,
            );
            scan_skill_root(
                &home.join(".codex/skills"),
                "codex",
                &[],
                &mut catalog,
                &mut seen,
            );
        }
        let mut lines = String::new();
        for name in skills {
            let description = catalog
                .iter()
                .find(|pick| pick.name.eq_ignore_ascii_case(name))
                .map(|pick| pick.description.clone())
                .unwrap_or_default();
            if description.is_empty() {
                lines.push_str(&format!("- {name}\n"));
            } else {
                lines.push_str(&format!("- {name} — {}\n", truncate(&description, 160)));
            }
        }
        format!(
            "\n## skills worth reaching for\nThe reviewer flagged these installed skills as likely relevant — use them where they genuinely help, skip them where they don't:\n{lines}"
        )
    }

    fn executor_business_context(&self) -> String {
        if let Some(path) = self.client.config.context_file.as_deref() {
            if let Ok(text) = fs::read_to_string(path) {
                return text;
            }
        }
        BUSINESS_CONTEXT.to_owned()
    }

    fn build_executor_prompt(&self, item: &WorkItem, extra: &str, job: &jobs::Job) -> String {
        let config = &self.client.config;
        let url = format!(
            "{}/{}/browse/{}",
            config.base_url, config.workspace, item.key
        );
        let labels = if item.labels.is_empty() {
            "none".to_owned()
        } else {
            item.labels.join(", ")
        };
        let due = item.due.clone().unwrap_or_else(|| "none".to_owned());
        let description = if item.description.trim().is_empty() {
            "(no description on the item)"
        } else {
            item.description.trim()
        };
        let reviewer_note = if extra.is_empty() {
            String::new()
        } else {
            format!("\n## reviewer note\n{extra}\n")
        };
        // a self-documenting repo carries its own product/engineering context
        // (CLAUDE.md / AGENTS.md, auto-loaded by the agent CLIs from the
        // worktree) — only inject the embedded dossier when it doesn't, or
        // when the user explicitly pointed --context-file at one
        let stance = match job.stance {
            jobs::JobStance::Explore => explore_stance_section(&item.key),
            jobs::JobStance::Implement => String::new(),
        };
        let skills = self.skills_prompt_section(&job.skills);
        let repo_has_docs = repo_has_agent_docs(&job.repo);
        let context = if repo_has_docs && config.context_file.is_none() {
            String::new()
        } else {
            format!(
                "\n## business context\n{}\n",
                self.executor_business_context()
            )
        };
        format!(
            "# {key} · {title}\n\nstate {state:?} · priority {priority} · labels {labels} · due {due}\nplane: {url}\n\n## task\n{description}\n{reviewer_note}{stance}{skills}{context}{envelope}",
            key = item.key,
            title = item.title,
            state = item.state,
            priority = item.priority.as_plane(),
            envelope = executor_envelope(&job.branch, repo_has_docs),
        )
    }

    /// Kick off the fable-5 brief for a two-stage dispatch; the job waits in
    /// BRIEFING and finish_codex_job turns the brief into its prompt.md.
    fn generate_dispatch_brief(&mut self, item: &WorkItem, extra: &str, job_id: String) {
        let mut meta_prompt = self.build_meta_prompt(item);
        if !extra.is_empty() {
            meta_prompt.push_str(&format!(
                "\n\nReviewer note — reflect this in the brief: {extra}\n"
            ));
        }
        let explore = self
            .job_index_by_id(&job_id)
            .map(|index| self.agent_jobs[index].job.stance == jobs::JobStance::Explore)
            .unwrap_or(false);
        if explore {
            meta_prompt.push_str(
                "\n\nThis dispatch is a design/exploration pass, not implementation: the brief should ask the agent for assumptions, unknown unknowns, architecture-changing questions, and prototyped options with a recommendation — not code changes.\n",
            );
        }
        let skills = self
            .job_index_by_id(&job_id)
            .map(|index| self.agent_jobs[index].job.skills.clone())
            .unwrap_or_default();
        if !skills.is_empty() {
            meta_prompt.push_str(&format!(
                "\n\nThe executor has these skills installed and the reviewer flagged them as relevant — the brief may reference them by name: {}.\n",
                skills.join(", ")
            ));
        }
        let item_key = item.key.clone();
        let config = &self.client.config;
        let backend = config.agent_backend;
        let out_file = std::env::temp_dir().join(format!(
            "plane-tui-dispatch-brief-{}-{item_key}.md",
            std::process::id()
        ));
        let child = match spawn_agent(config, &out_file) {
            Ok(child) => child,
            Err(err) => {
                self.fail_dispatch_brief(&job_id, &format!("brief spawn failed: {err:#}"));
                return;
            }
        };
        let pid = child.id();
        let agent_bin = config.agent_bin().to_owned();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let t0 = Instant::now();
            let prompt = complete_agent(child, backend, &agent_bin, &out_file, &meta_prompt);
            let _ = tx.send(CodexOutcome {
                prompt,
                comment: None,
                elapsed_ms: t0.elapsed().as_millis(),
            });
        });
        self.codex_job = Some(CodexJob {
            key: item_key,
            backend,
            comment_path: String::new(),
            pid,
            started: Instant::now(),
            rx,
            for_dispatch: Some(job_id),
        });
    }

    fn fail_dispatch_brief(&mut self, job_id: &str, note: &str) {
        if let Some(index) = self.job_index_by_id(job_id) {
            self.agent_jobs[index].job.status = jobs::JobStatus::Failed;
            self.agent_jobs[index].tail.push(note.to_owned());
            let _ = jobs::save(&self.agent_jobs[index].dir, &self.agent_jobs[index].job);
            self.status = format!(
                "✗ {}: {note} — x discards, or d to dispatch again",
                self.agent_jobs[index].job.item_key
            );
            self.force_clear = true;
        }
    }

    fn finish_dispatch_brief(&mut self, key: &str, job_id: &str, outcome: CodexOutcome) {
        let Some(index) = self.job_index_by_id(job_id) else {
            return;
        };
        match outcome.prompt {
            Ok(brief) => {
                let dir = self.agent_jobs[index].dir.clone();
                let branch = self.agent_jobs[index].job.branch.clone();
                let repo_has_docs = repo_has_agent_docs(&self.agent_jobs[index].job.repo);
                let stance = match self.agent_jobs[index].job.stance {
                    jobs::JobStance::Explore => {
                        explore_stance_section(&self.agent_jobs[index].job.item_key)
                    }
                    jobs::JobStance::Implement => String::new(),
                };
                let skills = self.skills_prompt_section(&self.agent_jobs[index].job.skills.clone());
                let prompt = format!(
                    "{}\n{stance}{skills}{}",
                    brief.trim(),
                    executor_envelope(&branch, repo_has_docs)
                );
                if let Err(err) = fs::write(dir.join("prompt.md"), prompt) {
                    self.fail_dispatch_brief(job_id, &format!("brief write failed: {err:#}"));
                    return;
                }
                self.agent_jobs[index].job.status = jobs::JobStatus::Queued;
                let _ = jobs::save(&self.agent_jobs[index].dir, &self.agent_jobs[index].job);
                self.api_log.push(ApiLog::new(
                    "AGENT",
                    key,
                    "dispatch brief",
                    "ok",
                    outcome.elapsed_ms,
                ));
                self.status = format!("brief ready — {key} queued for execution");
                self.force_clear = true;
            }
            Err(err) => {
                self.api_log.push(ApiLog::new(
                    "AGENT",
                    key,
                    "dispatch brief",
                    "err",
                    outcome.elapsed_ms,
                ));
                self.fail_dispatch_brief(job_id, &format!("brief generation failed: {err:#}"));
            }
        }
    }

    /// Enter a job's tmux pane: switch-client when resident, spawned terminal
    /// otherwise, clipboard as the last resort.
    fn deep_dive_job(&mut self, job: &jobs::Job) {
        match job.status {
            jobs::JobStatus::Queued => {
                self.status = format!(
                    "{} hasn't started yet — it's queued for an agent slot",
                    job.item_key
                );
                return;
            }
            jobs::JobStatus::Briefing => {
                self.status = format!(
                    "{} is still being briefed — the session starts after the brief",
                    job.item_key
                );
                return;
            }
            _ => {}
        }
        if !jobs::session_alive(job) {
            let hint = match job.status {
                jobs::JobStatus::Failed | jobs::JobStatus::Orphaned => " · r respawns it",
                jobs::JobStatus::Landed | jobs::JobStatus::Discarded => {
                    " (cleaned up on land/discard)"
                }
                _ => " (tmux server restarted?) — log and result are still in the job dir",
            };
            self.status = format!(
                "{}'s pane is gone — {} no longer exists{hint}",
                job.item_key, job.tmux_session
            );
            return;
        }
        let terminal_cmd = std::env::var("PLANE_TUI_TERMINAL_CMD").ok();
        let template = terminal_cmd.as_deref().unwrap_or("kitty -e {cmd}");
        match jobs::deep_dive(job, Some(template)) {
            Ok(jobs::DeepDive::Switched) => {
                self.status = format!("deep dive → {}", job.tmux_session);
            }
            Ok(jobs::DeepDive::SpawnedTerminal(cmd)) => {
                self.status = format!("deep dive → {cmd}");
            }
            Ok(jobs::DeepDive::CopyCommand(cmd)) => {
                let _ = copy_to_clipboard(&cmd);
                self.status = format!("attach command copied · {cmd}");
            }
            Err(err) => self.status = format!("deep dive failed: {err:#}"),
        }
    }

    /// One tick of fleet supervision: tail logs, settle finished jobs,
    /// drain background comment posts. Returns true when a redraw is due.
    fn pump_agent_jobs(&mut self) -> bool {
        let mut changed = false;
        // keep the WORKBENCH rail honest while the fleet is on screen —
        // throttled so the tmux round trip doesn't run every tick
        if self.jobs_open
            && self
                .work_sessions_at
                .map_or(true, |at| at.elapsed() >= Duration::from_secs(2))
        {
            changed |= self.refresh_work_sessions();
        }
        let mut transitions = Vec::new();
        for (index, handle) in self.agent_jobs.iter_mut().enumerate() {
            let before = handle.job.status;
            if jobs::pump(handle) {
                changed = true;
            }
            if handle.job.status != before {
                transitions.push(index);
            }
        }
        for index in transitions {
            self.on_job_transition(index);
        }
        // start queued jobs as slots free
        while self.running_agents() < agent_wip() {
            let Some(next) = self
                .agent_jobs
                .iter()
                .position(|handle| handle.job.status == jobs::JobStatus::Queued)
            else {
                break;
            };
            let key = self.agent_jobs[next].job.item_key.clone();
            let interactive = self.agent_jobs[next].job.mode == jobs::JobMode::Interactive;
            changed = true;
            match self.spawn_agent_job(next) {
                Ok(()) if interactive => {
                    self.status =
                        format!("▶ interactive session for {key} started — J then t to enter");
                }
                Ok(()) => self.status = format!("▶ {key} started — agent slot freed"),
                Err(err) => self.status = format!("could not start {key}: {err:#}"),
            }
        }
        // stall + hard-timeout supervision (spec §8 failure modes)
        let stall_min = env_u64("PLANE_TUI_STALL_MIN", 8);
        let timeout_min = env_u64("PLANE_TUI_JOB_TIMEOUT_MIN", 45);
        let mut supervision_note = None;
        for handle in &mut self.agent_jobs {
            if handle.job.status != jobs::JobStatus::Running {
                continue;
            }
            if handle.job.mode == jobs::JobMode::Interactive {
                continue; // human-paced: no stall or timeout supervision
            }
            let timed_out = timeout_min > 0
                && handle
                    .job
                    .started_at
                    .as_deref()
                    .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                    .map(|started| {
                        Utc::now().signed_duration_since(started)
                            > chrono::Duration::minutes(timeout_min as i64)
                    })
                    .unwrap_or(false);
            if timed_out {
                jobs::kill_session(&handle.job);
                handle.job.status = jobs::JobStatus::Failed;
                handle.tail.push(format!(
                    "── hard timeout after {timeout_min}m — cancelled ──"
                ));
                let _ = jobs::save(&handle.dir, &handle.job);
                supervision_note = Some(format!(
                    "✗ {} timed out after {timeout_min}m",
                    handle.job.item_key
                ));
                changed = true;
                continue;
            }
            if stall_min > 0
                && !handle.stalled
                && handle
                    .last_activity
                    .map(|at| at.elapsed() > Duration::from_secs(stall_min * 60))
                    .unwrap_or(false)
            {
                handle.stalled = true;
                supervision_note = Some(format!(
                    "⚠ {} quiet for {stall_min}m — J then t to look, c to cancel",
                    handle.job.item_key
                ));
                changed = true;
            }
        }
        if let Some(note) = supervision_note {
            self.status = note;
        }
        let mut posted = Vec::new();
        self.post_results.retain(|rx| match rx.try_recv() {
            Ok(outcome) => {
                posted.push(outcome);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        for (key, outcome) in posted {
            changed = true;
            match outcome {
                Ok(()) => self.api_log.push(ApiLog::new(
                    "POST",
                    &format!("agent summary comment → {key}"),
                    "",
                    "201",
                    0,
                )),
                Err(err) => self.status = format!("comment post failed for {key}: {err}"),
            }
        }
        changed
    }

    fn on_job_transition(&mut self, index: usize) {
        let (key, status, dir, job) = {
            let handle = &mut self.agent_jobs[index];
            if matches!(
                handle.job.status,
                jobs::JobStatus::Review | jobs::JobStatus::Question
            ) {
                handle.diff_stat = Some(jobs::diff_stat(&handle.job));
            }
            (
                handle.job.item_key.clone(),
                handle.job.status,
                handle.dir.clone(),
                handle.job.clone(),
            )
        };
        match status {
            jobs::JobStatus::Review => {
                self.status = format!("⚑ {key} agent finished — J to review");
                self.post_result_comment(&job, &dir);
            }
            jobs::JobStatus::Question => {
                self.status = format!("? {key} agent has a question — J to read");
                self.post_result_comment(&job, &dir);
            }
            jobs::JobStatus::Failed => {
                self.status = format!("✗ {key} agent failed — J for the log");
            }
            jobs::JobStatus::Orphaned => {
                self.status = format!("{key} agent orphaned (tmux session gone) — J to retry");
            }
            _ => {}
        }
        self.force_clear = true;
    }

    /// Post the agent's result back to the Plane item as a comment, in the
    /// background with retries — the spec's "TUI is the only Plane writer".
    fn post_result_comment(&mut self, job: &jobs::Job, dir: &std::path::Path) {
        let result = jobs::read_result(dir);
        if result.trim().is_empty() {
            return;
        }
        let client = self.client.clone();
        let project_id = job.project_id.clone();
        let item_id = job.item_id.clone();
        let key = job.item_key.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let body = json!({
                "comment_html": format!("<pre>{}</pre>", escape_html(result.trim())),
            });
            let mut outcome = Err("not attempted".to_owned());
            for attempt in 0u32..3 {
                match client.create_comment(&project_id, &item_id, body.clone()) {
                    Ok(_) => {
                        outcome = Ok(());
                        break;
                    }
                    Err(err) => outcome = Err(format!("{err:#}")),
                }
                thread::sleep(Duration::from_secs(2u64 << attempt));
            }
            let _ = tx.send((key, outcome));
        });
        self.post_results.push(rx);
    }

    fn job_badge(&self, key: &str) -> Option<(&'static str, Color)> {
        let handle = self
            .agent_jobs
            .iter()
            .rev()
            .find(|handle| handle.job.item_key == key && handle.job.status.is_active())?;
        Some(match handle.job.status {
            jobs::JobStatus::Briefing => ("✎", theme().amber),
            jobs::JobStatus::Queued => ("●", theme().dimmer),
            jobs::JobStatus::Running if handle.stalled => ("⚠", theme().amber),
            jobs::JobStatus::Running => ("⚑", theme().green),
            jobs::JobStatus::Review => ("⚑", theme().amber),
            jobs::JobStatus::Question => ("?", theme().amber),
            jobs::JobStatus::Failed | jobs::JobStatus::Orphaned => ("✗", theme().red),
            _ => return None,
        })
    }

    /// Fleet display order: things needing a human sort above things working.
    fn fleet_order(&self) -> Vec<usize> {
        fn rank(status: jobs::JobStatus) -> u8 {
            match status {
                jobs::JobStatus::Question => 0,
                jobs::JobStatus::Review => 1,
                jobs::JobStatus::Failed => 2,
                jobs::JobStatus::Orphaned => 3,
                jobs::JobStatus::Running => 4,
                jobs::JobStatus::Briefing => 5,
                jobs::JobStatus::Queued => 6,
                jobs::JobStatus::Landed => 7,
                jobs::JobStatus::Discarded => 8,
            }
        }
        let mut order: Vec<usize> = (0..self.agent_jobs.len()).collect();
        order.sort_by(|&a, &b| {
            let left = &self.agent_jobs[a];
            let right = &self.agent_jobs[b];
            rank(left.job.status)
                .cmp(&rank(right.job.status))
                .then(right.job.id.cmp(&left.job.id))
        });
        order
    }

    /// Fleet selection space: dispatched jobs in rail order, then live `w`
    /// work sessions as "work:<session>" ids.
    fn fleet_ids(&self, order: &[usize]) -> Vec<String> {
        let mut ids: Vec<String> = order
            .iter()
            .map(|&i| self.agent_jobs[i].job.id.clone())
            .collect();
        ids.extend(
            self.work_sessions
                .iter()
                .map(|session| format!("work:{}", session.session)),
        );
        ids
    }

    fn handle_jobs_key(&mut self, key: KeyEvent) -> Result<()> {
        // selection tracks the JOB, not a row number: the list reorders
        // itself on status transitions, and a positional index would let a
        // keypress land on a different job than the one on screen
        let order = self.fleet_order();
        let ids = self.fleet_ids(&order);
        let current_pos = self
            .jobs_sel_id
            .as_deref()
            .and_then(|id| ids.iter().position(|have| have == id))
            .unwrap_or(0);
        if let Some(id) = ids.get(current_pos) {
            self.jobs_sel_id = Some(id.clone());
        }
        // exactly one of these is Some: a job row or a WORKBENCH row
        let selected: Option<usize> = (current_pos < order.len()).then(|| order[current_pos]);
        let selected_work: Option<usize> = current_pos
            .checked_sub(order.len())
            .filter(|&work| work < self.work_sessions.len());
        match key.code {
            KeyCode::Esc | KeyCode::Char('J') | KeyCode::Char('q') => {
                self.jobs_open = false;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let pos = min(current_pos + 1, ids.len().saturating_sub(1));
                self.jobs_sel_id = ids.get(pos).cloned();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let pos = current_pos.saturating_sub(1);
                self.jobs_sel_id = ids.get(pos).cloned();
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.jobs_sel_id = ids.first().cloned();
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.jobs_sel_id = ids.last().cloned();
            }
            KeyCode::Char('t') => {
                if let Some(index) = selected {
                    let job = self.agent_jobs[index].job.clone();
                    self.deep_dive_job(&job);
                } else if let Some(work) = selected_work {
                    let session = self.work_sessions[work].session.clone();
                    self.enter_work_session(&session);
                }
            }
            KeyCode::Enter => {
                if let Some(index) = selected {
                    let job = self.agent_jobs[index].job.clone();
                    if matches!(
                        job.status,
                        jobs::JobStatus::Review | jobs::JobStatus::Question
                    ) {
                        let result = self.view_diff_in_pager(&job);
                        self.soft(result);
                    }
                }
            }
            KeyCode::Char('f') => {
                if let Some(index) = selected {
                    let job = self.agent_jobs[index].job.clone();
                    if matches!(
                        job.status,
                        jobs::JobStatus::Review
                            | jobs::JobStatus::Question
                            | jobs::JobStatus::Failed
                            | jobs::JobStatus::Orphaned
                    ) {
                        self.feedback_job = Some(job.id.clone());
                        self.feedback_backend = None;
                        self.jobs_open = false;
                        self.menu = Some(MenuMode::Feedback);
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(index) = selected {
                    let job_status = self.agent_jobs[index].job.status;
                    if job_status == jobs::JobStatus::Running {
                        let handle = &mut self.agent_jobs[index];
                        jobs::kill_session(&handle.job);
                        handle.job.status = jobs::JobStatus::Failed;
                        handle.tail.push("── cancelled by you ──".to_owned());
                        let _ = jobs::save(&handle.dir, &handle.job);
                        self.status = format!("{} cancelled — r retries", handle.job.item_key);
                    } else if job_status == jobs::JobStatus::Queued {
                        let job = self.agent_jobs[index].job.clone();
                        match jobs::discard(&job) {
                            Ok(()) => {
                                self.agent_jobs[index].job.status = jobs::JobStatus::Discarded;
                                let _ = jobs::save(
                                    &self.agent_jobs[index].dir,
                                    &self.agent_jobs[index].job,
                                );
                                self.status = format!("{} removed from the queue", job.item_key);
                            }
                            Err(err) => self.status = format!("cancel failed: {err:#}"),
                        }
                    }
                } else if let Some(work) = selected_work {
                    self.end_work_session(work);
                }
            }
            KeyCode::Char('r') => {
                if let Some(index) = selected {
                    let handle = &mut self.agent_jobs[index];
                    if matches!(
                        handle.job.status,
                        jobs::JobStatus::Failed | jobs::JobStatus::Orphaned
                    ) {
                        if !handle.dir.join("prompt.md").exists() {
                            self.status =
                                "no prompt on this job (its brief failed) — x discard, then d again"
                                    .to_owned();
                            self.force_clear = true;
                            return Ok(());
                        }
                        jobs::kill_session(&handle.job);
                        handle.job.attempt += 1;
                        handle.job.tmux_session =
                            jobs::session_name(&handle.job.item_key, handle.job.attempt);
                        handle
                            .tail
                            .push(format!("── attempt {} ──", handle.job.attempt));
                        let key = handle.job.item_key.clone();
                        let attempt = handle.job.attempt;
                        match self.spawn_agent_job(index) {
                            Ok(()) => {
                                self.status = format!("{key} retrying (attempt {attempt})");
                            }
                            Err(err) => self.status = format!("retry failed: {err:#}"),
                        }
                    }
                }
            }
            KeyCode::Char('l') => {
                if let Some(index) = selected {
                    let job = self.agent_jobs[index].job.clone();
                    if matches!(
                        job.status,
                        jobs::JobStatus::Review | jobs::JobStatus::Question
                    ) {
                        self.land_job = Some(job.id.clone());
                        self.jobs_open = false;
                        self.menu = Some(MenuMode::Land);
                    }
                }
            }
            KeyCode::Char('x') => {
                if let Some(index) = selected {
                    let job = self.agent_jobs[index].job.clone();
                    if matches!(
                        job.status,
                        jobs::JobStatus::Review
                            | jobs::JobStatus::Question
                            | jobs::JobStatus::Failed
                            | jobs::JobStatus::Orphaned
                            | jobs::JobStatus::Queued
                            | jobs::JobStatus::Landed
                    ) {
                        let was_landed = job.status == jobs::JobStatus::Landed;
                        match jobs::discard(&job) {
                            Ok(()) => {
                                self.agent_jobs[index].job.status = jobs::JobStatus::Discarded;
                                let _ = jobs::save(
                                    &self.agent_jobs[index].dir,
                                    &self.agent_jobs[index].job,
                                );
                                if was_landed {
                                    let item_key = job.item_key.clone();
                                    match self.with_single_target(&item_key, |app| {
                                        app.apply_state(StateKind::Started)
                                    }) {
                                        Ok(()) => {
                                            self.status = format!(
                                                "{} landed job discarded · item reopened for redo",
                                                job.item_key
                                            );
                                        }
                                        Err(err) => {
                                            self.status = format!(
                                                "{} discarded locally, but reopen failed: {err:#}",
                                                job.item_key
                                            );
                                        }
                                    }
                                } else {
                                    self.status = format!(
                                        "{} discarded · branch {} deleted",
                                        job.item_key, job.branch
                                    );
                                }
                            }
                            Err(err) => self.status = format!("discard failed: {err:#}"),
                        }
                    }
                } else if let Some(work) = selected_work {
                    self.end_work_session(work);
                }
            }
            _ => {}
        }
        self.force_clear = true;
        Ok(())
    }

    fn end_work_session(&mut self, work: usize) {
        let Some(session) = self.work_sessions.get(work).cloned() else {
            return;
        };
        jobs::kill_session_raw(&jobs::default_socket(), &session.session);
        self.refresh_work_sessions();
        self.status = format!("{} work session ended", session.item_key);
    }

    fn draw_fleet(&self, out: &mut Screen, x: u16, y: u16, width: u16, height: u16) -> Result<()> {
        let order = self.fleet_order();

        // ── health summary bar ───────────────────────────────────────────
        let (mut running, mut queued, mut needs, mut failed) = (0usize, 0usize, 0usize, 0usize);
        for &i in &order {
            let st = self.agent_jobs[i].job.status;
            match st {
                jobs::JobStatus::Running => running += 1,
                jobs::JobStatus::Queued => queued += 1,
                _ => {}
            }
            if matches!(st, jobs::JobStatus::Failed | jobs::JobStatus::Orphaned) {
                failed += 1;
            }
            if FleetBucket::of(st) == FleetBucket::NeedsYou {
                needs += 1;
            }
        }
        draw_cell(out, x, y, width, "", theme().dim, Some(theme().bg), false)?;
        let mut hx = x + 1;
        draw_span(out, &mut hx, y, "fleet", theme().paper, Some(theme().bg), true)?;
        draw_span(out, &mut hx, y, "   ", theme().dim, Some(theme().bg), false)?;
        draw_span(
            out,
            &mut hx,
            y,
            &format!("running {running}/{}", agent_wip()),
            if running > 0 { theme().green } else { theme().dim },
            Some(theme().bg),
            false,
        )?;
        let yours = self.work_sessions.len();
        for (text, color, on) in [
            (format!("{needs} need you"), theme().amber, needs > 0),
            (format!("{failed} failed"), theme().red, failed > 0),
            (format!("{queued} queued"), theme().accent, queued > 0),
            (format!("{yours} yours"), theme().green, yours > 0),
        ] {
            draw_span(out, &mut hx, y, "  ·  ", theme().dimmer, Some(theme().bg), false)?;
            draw_span(
                out,
                &mut hx,
                y,
                &text,
                if on { color } else { theme().dim },
                Some(theme().bg),
                on,
            )?;
        }
        let mut dx = x;
        draw_span(
            out,
            &mut dx,
            y + 1,
            &"─".repeat(width as usize),
            theme().line,
            Some(theme().bg),
            false,
        )?;

        let list_top = y + 2;
        let list_height = height.saturating_sub(2);
        if order.is_empty() && self.work_sessions.is_empty() {
            draw_cell(
                out,
                x + 1,
                list_top,
                width.saturating_sub(2),
                "no agents yet — d dispatches one · w opens a hands-on work session",
                theme().dim,
                Some(theme().bg),
                false,
            )?;
            return Ok(());
        }

        let show_detail = width >= 96;
        let left_width = if show_detail {
            (width * 42 / 100).clamp(46, 70)
        } else {
            width
        };
        if show_detail {
            for r in 0..list_height {
                let mut vx = x + left_width;
                draw_span(out, &mut vx, list_top + r, "│", theme().line, Some(theme().bg), false)?;
            }
        }

        // ── grouped, scroll-windowed job rail ────────────────────────────
        enum FleetRow {
            Header(FleetBucket, usize),
            Job(usize),
            WorkHeader(usize),
            Work(usize),
        }
        let mut bcount = [0usize; 4];
        for &i in &order {
            bcount[FleetBucket::of(self.agent_jobs[i].job.status) as usize] += 1;
        }
        let mut rows: Vec<FleetRow> = Vec::new();
        let mut last: Option<FleetBucket> = None;
        for (pos, &i) in order.iter().enumerate() {
            let bucket = FleetBucket::of(self.agent_jobs[i].job.status);
            if last != Some(bucket) {
                rows.push(FleetRow::Header(bucket, bcount[bucket as usize]));
                last = Some(bucket);
            }
            rows.push(FleetRow::Job(pos));
        }
        if !self.work_sessions.is_empty() {
            rows.push(FleetRow::WorkHeader(self.work_sessions.len()));
            for work in 0..self.work_sessions.len() {
                rows.push(FleetRow::Work(work));
            }
        }
        let ids = self.fleet_ids(&order);
        let sel_pos = self
            .jobs_sel_id
            .as_deref()
            .and_then(|id| ids.iter().position(|have| have == id))
            .unwrap_or(0);
        let sel_display = rows
            .iter()
            .position(|row| match row {
                FleetRow::Job(pos) => *pos == sel_pos,
                FleetRow::Work(work) => order.len() + *work == sel_pos,
                _ => false,
            })
            .unwrap_or(0);
        let cap = list_height as usize;
        let start = if cap > 0 && sel_display >= cap {
            sel_display + 1 - cap
        } else {
            0
        };
        let end = min(start + cap, rows.len());
        let rail_width = left_width.saturating_sub(1);
        let mut r = list_top;
        for row in &rows[start..end] {
            match row {
                FleetRow::Header(bucket, n) => draw_cell(
                    out,
                    x + 1,
                    r,
                    rail_width,
                    &format!("{} · {n}", bucket.title()),
                    bucket.color(),
                    Some(theme().bg),
                    true,
                )?,
                FleetRow::Job(pos) => {
                    let handle = &self.agent_jobs[order[*pos]];
                    let job = &handle.job;
                    let stalled = handle.stalled && job.status == jobs::JobStatus::Running;
                    let glyph = fleet_glyph(job.status, stalled, self.frame);
                    let label = if stalled {
                        "STALLED?"
                    } else {
                        job.status.label()
                    };
                    let model_col = if job.mode == jobs::JobMode::Interactive {
                        format!("{}·int", job.backend)
                    } else {
                        format!("{}·{}", job.backend, truncate(&job.model, 7))
                    };
                    let selected = pos == &sel_pos;
                    let text = format!(
                        "  {glyph} {:<10} {:<9} {} a{}",
                        job.item_key, label, model_col, job.attempt
                    );
                    let (fg, bg) = if selected {
                        (theme().ink, Some(theme().paper))
                    } else {
                        (fleet_color(job.status, stalled), Some(theme().bg))
                    };
                    draw_cell(out, x + 1, r, rail_width, &text, fg, bg, selected)?;
                }
                FleetRow::WorkHeader(n) => draw_cell(
                    out,
                    x + 1,
                    r,
                    rail_width,
                    &format!("WORKBENCH · {n}"),
                    theme().green,
                    Some(theme().bg),
                    true,
                )?,
                FleetRow::Work(work) => {
                    let session = &self.work_sessions[*work];
                    let folder = session
                        .cwd
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| session.cwd.display().to_string());
                    let selected = order.len() + work == sel_pos;
                    let text = format!(
                        "  ◆ {:<10} {:<9} you       {}",
                        session.item_key,
                        "OPEN",
                        truncate(&folder, 18),
                    );
                    let (fg, bg) = if selected {
                        (theme().ink, Some(theme().paper))
                    } else {
                        (theme().green, Some(theme().bg))
                    };
                    draw_cell(out, x + 1, r, rail_width, &text, fg, bg, selected)?;
                }
            }
            r += 1;
        }
        if start > 0 || end < rows.len() {
            let above = rows[..start]
                .iter()
                .filter(|r| matches!(r, FleetRow::Job(_) | FleetRow::Work(_)))
                .count();
            let below = rows[end..]
                .iter()
                .filter(|r| matches!(r, FleetRow::Job(_) | FleetRow::Work(_)))
                .count();
            let hint = match (above, below) {
                (a, b) if a > 0 && b > 0 => format!("  ↑ {a} · ↓ {b} more"),
                (a, _) if a > 0 => format!("  ↑ {a} more"),
                (_, b) => format!("  ↓ {b} more"),
            };
            draw_cell(
                out,
                x + 1,
                list_top + list_height.saturating_sub(1),
                rail_width,
                &hint,
                theme().dimmer,
                Some(theme().bg),
                false,
            )?;
        }

        if show_detail {
            let dx = x + left_width + 2;
            let dwidth = width.saturating_sub(left_width + 3);
            if sel_pos < order.len() {
                self.draw_fleet_detail(out, dx, list_top, dwidth, list_height, order[sel_pos])?;
            } else if let Some(session) = self.work_sessions.get(sel_pos - order.len()) {
                self.draw_work_detail(out, dx, list_top, dwidth, list_height, session)?;
            }
        }
        Ok(())
    }

    fn draw_work_detail(
        &self,
        out: &mut Screen,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        session: &WorkSession,
    ) -> Result<()> {
        let bottom = y + height;
        let mut row = y;
        draw_cell(
            out,
            x,
            row,
            width,
            &format!("{} · work session", session.item_key),
            theme().paper,
            Some(theme().bg),
            true,
        )?;
        row += 1;
        for (text, color) in [
            (
                "human-driven — no worktree, changes land wherever you make them".to_owned(),
                theme().dim,
            ),
            (format!("folder {}", session.cwd.display()), theme().dimmer),
            (format!("tmux   {}", session.session), theme().dimmer),
            (String::new(), theme().dim),
            ("t enter the pane · c/x end the session".to_owned(), theme().text),
        ] {
            if row >= bottom {
                break;
            }
            draw_cell(
                out,
                x,
                row,
                width,
                &truncate(&text, width as usize),
                color,
                Some(theme().bg),
                false,
            )?;
            row += 1;
        }
        Ok(())
    }

    fn draw_fleet_detail(
        &self,
        out: &mut Screen,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        index: usize,
    ) -> Result<()> {
        let handle = &self.agent_jobs[index];
        let job = &handle.job;
        let bottom = y + height;
        let mut row = y;
        let key_room = width.saturating_sub(job.item_key.len() as u16 + 3) as usize;
        draw_cell(
            out,
            x,
            row,
            width,
            &format!("{} · {}", job.item_key, truncate(&job.title, key_room)),
            theme().paper,
            Some(theme().bg),
            true,
        )?;
        row += 1;
        let stance = if job.stance == jobs::JobStance::Explore {
            "explore"
        } else {
            "implement"
        };
        for (text, color) in [
            (
                format!("{} · {stance} · attempt {}", job.backend, job.attempt),
                theme().dim,
            ),
            (
                format!(
                    "branch {}",
                    truncate(&job.branch, width.saturating_sub(7) as usize)
                ),
                theme().dimmer,
            ),
            (format!("{}", job.worktree.display()), theme().dimmer),
        ] {
            if row >= bottom {
                break;
            }
            draw_cell(
                out,
                x,
                row,
                width,
                &truncate(&text, width as usize),
                color,
                Some(theme().bg),
                false,
            )?;
            row += 1;
        }
        if row < bottom {
            let mut sx = x;
            draw_span(
                out,
                &mut sx,
                row,
                &"─".repeat(width as usize),
                theme().line,
                Some(theme().bg),
                false,
            )?;
            row += 1;
        }

        let mut lines: Vec<(String, Color)> = Vec::new();
        match job.status {
            jobs::JobStatus::Review | jobs::JobStatus::Question => {
                for l in jobs::read_result(&handle.dir).trim().lines() {
                    lines.push((l.to_owned(), theme().text));
                }
                if let Some(diff) = &handle.diff_stat {
                    lines.push((String::new(), theme().dim));
                    lines.push(("diff:".to_owned(), theme().dim));
                    for l in diff.lines() {
                        lines.push((l.to_owned(), theme().green));
                    }
                }
            }
            jobs::JobStatus::Running if job.mode == jobs::JobMode::Interactive => {
                lines.push((
                    format!(
                        "interactive {} session — press t to enter the pane",
                        job.backend
                    ),
                    theme().text,
                ));
                lines.push(("(the pane scrollback is the record)".to_owned(), theme().dimmer));
            }
            _ => {
                for l in &handle.tail {
                    lines.push((l.clone(), theme().text));
                }
            }
        }
        let available = bottom.saturating_sub(row) as usize;
        let skip = lines.len().saturating_sub(available);
        for (text, color) in lines.into_iter().skip(skip) {
            if row >= bottom {
                break;
            }
            draw_cell(
                out,
                x,
                row,
                width,
                &truncate(&text, width as usize),
                color,
                Some(theme().bg),
                false,
            )?;
            row += 1;
        }
        Ok(())
    }

    /// Surface an error on the status line instead of letting it unwind the
    /// event loop — a failed PATCH must never take the whole TUI down.
    fn soft(&mut self, result: Result<()>) {
        if let Err(err) = result {
            self.status = format!("error: {err:#}");
            self.force_clear = true;
        }
    }

    fn soft_or<T>(&mut self, result: Result<T>, fallback: T) -> T {
        match result {
            Ok(value) => value,
            Err(err) => {
                self.status = format!("error: {err:#}");
                self.force_clear = true;
                fallback
            }
        }
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
        if self.skill_wizard.is_some() {
            return self.handle_skill_wizard_key(key);
        }
        if self.repo_wizard.is_some() {
            return self.handle_repo_wizard_key(key);
        }
        if self.work_wizard.is_some() {
            return self.handle_work_wizard_key(key);
        }
        if self.jobs_open {
            return self.handle_jobs_key(key);
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
            KeyCode::Char('d') => self.start_dispatch(),
            KeyCode::Char('w') => self.open_work_wizard(),
            KeyCode::Char('J') => {
                self.jobs_open = true;
                self.jobs_sel_id = None;
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
            KeyCode::Char('R') => {
                let result = self.refresh();
                self.soft(result);
            }
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
            KeyCode::Char('C') => self.cycle_theme(),
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
            if matches!(self.menu, Some(MenuMode::Feedback) | Some(MenuMode::Land)) {
                self.jobs_open = true; // back to the fleet, nothing done
            }
            self.menu = None;
            self.dispatch_item = None;
            self.feedback_job = None;
            self.land_job = None;
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
                            let result = self.apply_state(state);
                            self.soft(result);
                            self.menu = None;
                            self.marks.clear();
                            self.force_clear = true;
                        }
                    }
                }
            }
            MenuMode::Dispatch => {
                let chosen = match key.code {
                    KeyCode::Enter => Some(self.dispatch_backend),
                    KeyCode::Char('1') => Some(AgentBackend::Codex),
                    KeyCode::Char('2') => Some(AgentBackend::Claude),
                    KeyCode::Char('i') => {
                        self.dispatch_interactive = !self.dispatch_interactive;
                        self.force_clear = true;
                        None
                    }
                    KeyCode::Char('b') => {
                        self.dispatch_brief = !self.dispatch_brief;
                        self.force_clear = true;
                        None
                    }
                    KeyCode::Char('e') => {
                        self.dispatch_explore = !self.dispatch_explore;
                        self.force_clear = true;
                        None
                    }
                    KeyCode::Char('s') => {
                        self.menu = None;
                        self.open_skill_wizard();
                        None
                    }
                    KeyCode::Char('r') => {
                        let count = repo_registry(&self.client.config).len().max(1);
                        self.dispatch_repo = (self.dispatch_repo + 1) % count;
                        self.force_clear = true;
                        None
                    }
                    KeyCode::Char('R') => {
                        self.menu = None;
                        self.dispatch_item = None;
                        self.open_repo_wizard();
                        self.status = "repo wizard — press d again after picking".to_owned();
                        None
                    }
                    _ => None,
                };
                if let Some(backend) = chosen {
                    self.dispatch_backend = backend;
                    self.menu = None;
                    self.input_mode = Some(InputMode::DispatchExtra);
                    self.input.clear();
                    self.input_cursor = 0;
                    self.force_clear = true;
                }
            }
            MenuMode::Feedback => {
                let choice = match key.code {
                    KeyCode::Char('1') | KeyCode::Enter => Some(None),
                    KeyCode::Char('2') => Some(Some(AgentBackend::Codex)),
                    KeyCode::Char('3') => Some(Some(AgentBackend::Claude)),
                    _ => None,
                };
                if let Some(backend) = choice {
                    self.feedback_backend = backend;
                    self.menu = None;
                    self.input_mode = Some(InputMode::FeedbackNote);
                    self.input.clear();
                    self.input_cursor = 0;
                    self.force_clear = true;
                }
            }
            MenuMode::Land => {
                let how = match key.code {
                    KeyCode::Char('m') => Some('m'),
                    KeyCode::Char('P') => Some('P'),
                    KeyCode::Char('b') => Some('b'),
                    _ => None,
                };
                if let Some(how) = how {
                    self.menu = None;
                    self.force_clear = true;
                    self.land_selected(how);
                }
            }
            MenuMode::ConfirmWip => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let result = self.apply_state(StateKind::Started);
                    self.soft(result);
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
                        let result = self.apply_priority(priority);
                        self.soft(result);
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
                        let result = self.toggle_label(index as usize);
                        self.soft(result);
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
        self.invalidate_screen();
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
                self.work_item = None;
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
                    InputMode::Command => {
                        let result = self.run_command();
                        self.soft_or(result, false)
                    }
                    InputMode::NewLabel => {
                        let result = self.create_label_from_input();
                        self.soft(result);
                        false
                    }
                    InputMode::BackendModel => self.update_backend_wizard_model(),
                    InputMode::BackendEffort => self.update_backend_wizard_effort(),
                    InputMode::ProjectName => {
                        self.advance_project_wizard_name();
                        true
                    }
                    InputMode::ProjectIdentifier => {
                        let result = self.create_project_from_wizard();
                        self.soft_or(result, false)
                    }
                    InputMode::EditTitle => {
                        let result = self.apply_title_edit();
                        self.soft(result);
                        false
                    }
                    InputMode::EditDue => {
                        let result = self.apply_due_edit();
                        self.soft(result);
                        false
                    }
                    InputMode::DispatchExtra => {
                        let result = self.dispatch_job();
                        self.soft(result);
                        false
                    }
                    InputMode::FeedbackNote => {
                        let result = self.requeue_with_feedback();
                        self.soft(result);
                        false
                    }
                    InputMode::WorkFolder => {
                        let folder = expand_tilde(self.input.trim());
                        if let Some(item_key) = self.work_item.take() {
                            self.launch_work_session(&item_key, &folder);
                        }
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
        self.work_item = None;
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
            "repos" => {
                self.open_repo_wizard();
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
            "theme" | "colors" | "colorscheme" => {
                match rest {
                    "" | "next" => self.cycle_theme(),
                    "list" => {
                        let active = theme().name;
                        let names = THEMES
                            .iter()
                            .map(|t| {
                                if t.name == active {
                                    format!("[{}]", t.name)
                                } else {
                                    t.name.to_owned()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("  ");
                        self.status = format!("themes · {names}");
                    }
                    name => self.set_theme_by_name(name),
                }
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
        let key = format!("{}-{}", self.project().identifier, item.sequence_id);
        self.select_item_by_key(&key);
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
                let result = self
                    .with_single_target(&current_key, |app| app.apply_priority(Priority::Urgent));
                self.soft(result);
                advance = true;
            }
            KeyCode::Char('h') => {
                let result =
                    self.with_single_target(&current_key, |app| app.apply_priority(Priority::High));
                self.soft(result);
                advance = true;
            }
            KeyCode::Char('m') => {
                let result = self
                    .with_single_target(&current_key, |app| app.apply_priority(Priority::Medium));
                self.soft(result);
                advance = true;
            }
            KeyCode::Char('l') => {
                let result =
                    self.with_single_target(&current_key, |app| app.apply_priority(Priority::Low));
                self.soft(result);
                advance = true;
            }
            KeyCode::Char('n') => advance = true,
            KeyCode::Char('2') => {
                let result =
                    self.with_single_target(&current_key, |app| app.apply_state(StateKind::Todo));
                self.soft(result);
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
                    let result = self.with_single_target(&current_key, |app| {
                        app.apply_state(StateKind::Started)
                    });
                    self.soft(result);
                    advance = true;
                    promoted = true;
                }
            }
            KeyCode::Char('5') => {
                let result = self
                    .with_single_target(&current_key, |app| app.apply_state(StateKind::Cancelled));
                self.soft(result);
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
            for_dispatch: None,
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
             Write the brief you would want to receive if you were the agent taking on this task. The agent is a top-tier model: it will read the repo, find the right files, and make good design and process decisions on its own. The brief is an assignment (what and why), not an implementation plan (how).\n\
             Leave out everything the agent can figure out itself or that the dispatch harness already handles: no git/branch/commit instructions, no lists of files to touch, no step-by-step plans, no testing checklists, no restating repo conventions. Include only what the agent cannot derive from the repo: the goal, why it matters, and any non-obvious constraints or gotchas — with open questions flagged as assumptions to verify rather than guessed at.\n\
             Everything in the brief must be true and specific to this work item. Shorter is better; a few tight paragraphs usually beat a structured document.\n\n\
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

    /// Build a fresh frame, either blank (force_clear) or seeded from what's on
    /// screen (so untouched cells cost nothing in the diff), and hand it to the
    /// draw_* functions.
    fn new_frame(&mut self, width: u16, height: u16) -> Screen {
        let size_changed = self.screen.w != width || self.screen.h != height;
        let mut buf = if self.force_clear || size_changed {
            Screen::blank(width, height)
        } else {
            let mut buf = Screen::blank(width, height);
            buf.cells.clone_from(&self.screen.cells);
            buf
        };
        self.force_clear = false;
        buf.begin_frame();
        buf
    }

    /// Emit a diffed frame to the terminal. On a resize we first repaint the
    /// physical screen to the base background so the grid and terminal agree.
    fn present(&mut self, mut buf: Screen, width: u16, height: u16) -> Result<()> {
        let mut stdout = io::stdout();
        queue!(stdout, BeginSynchronizedUpdate, Hide)?;
        if self.screen.w != width || self.screen.h != height {
            queue!(
                stdout,
                SetBackgroundColor(theme().bg),
                Clear(ClearType::All),
                ResetColor
            )?;
            self.screen = Screen::blank(width, height);
        }
        buf.flush_into(&mut self.screen, &mut stdout)?;
        queue!(stdout, EndSynchronizedUpdate)?;
        stdout.flush()?;
        Ok(())
    }

    /// Forget what's on the terminal after an external program (a git pager or
    /// `$EDITOR`) took it over: the next frame then does a full physical repaint
    /// instead of diffing against a stale model.
    fn invalidate_screen(&mut self) {
        self.screen = Screen::default();
        self.force_clear = true;
    }

    /// Switch the active color scheme, persist the choice, and force a full
    /// repaint (every cell's background changes, so the diff model is reset).
    fn apply_theme(&mut self, scheme: Theme) {
        set_active_theme(scheme);
        save_theme_name(scheme.name);
        self.invalidate_screen();
        self.status = format!("theme · {}", scheme.name);
    }

    fn cycle_theme(&mut self) {
        self.apply_theme(next_theme(theme().name));
    }

    fn set_theme_by_name(&mut self, name: &str) {
        match theme_by_name(name) {
            Some(scheme) => self.apply_theme(scheme),
            None => {
                let names = THEMES
                    .iter()
                    .map(|t| t.name)
                    .collect::<Vec<_>>()
                    .join(", ");
                self.status = format!("unknown theme {name:?} · try: {names}");
            }
        }
    }

    fn draw(&mut self) -> Result<()> {
        let (width, height) = size()?;
        let mut buf = self.new_frame(width, height);
        let out = &mut buf;
        let frame = LayoutFrame::new(width, height);
        draw_outer_frame(out, frame)?;
        self.draw_header(out, frame.x, frame.width, frame.y)?;
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
        if self.jobs_open {
            // fleet is a full-screen workspace, not a modal
            self.draw_fleet(out, frame.x, body_top, frame.width, body_height)?;
        } else {
            match self.view {
                ViewMode::Board => {
                    self.draw_board(out, frame.x, body_top, board_width, body_height)?
                }
                ViewMode::List => {
                    self.draw_list(out, frame.x, body_top, board_width, body_height)?
                }
            }
            if inspector_width > 0 {
                self.draw_inspector(
                    out,
                    frame.x + board_width,
                    body_top,
                    inspector_width,
                    body_height,
                )?;
            }
        }
        self.draw_footer(
            out,
            frame.x,
            body_top + body_height,
            frame.width,
            footer_height,
        )?;
        if self.keys_open {
            self.draw_keys_overlay(out, width, height)?;
        }
        if self.notes_open {
            self.draw_notes_overlay(out, width, height)?;
        }
        if self.triage.is_some() {
            self.draw_triage_overlay(out, width, height)?;
        }
        if self.detail.is_some() {
            self.draw_detail_overlay(out, width, height)?;
        }
        if self.prompt_view.is_some() {
            self.draw_prompt_overlay(out, width, height)?;
        }
        if self.repo_wizard.is_some() {
            self.draw_repo_wizard(out, width, height)?;
        }
        if self.skill_wizard.is_some() {
            self.draw_skill_wizard(out, width, height)?;
        }
        if self.work_wizard.is_some() {
            self.draw_work_wizard(out, width, height)?;
        }
        self.present(buf, width, height)
    }

    fn draw_active_overlay(&mut self) -> Result<()> {
        let (width, height) = size()?;
        let mut buf = self.new_frame(width, height);
        if self.prompt_view.is_some() {
            self.draw_prompt_overlay_body(&mut buf, width, height)?;
        } else if self.detail.is_some() {
            self.draw_detail_overlay_body(&mut buf, width, height)?;
        }
        self.present(buf, width, height)
    }

    fn draw_footer_only(&mut self) -> Result<()> {
        let (width, height) = size()?;
        let mut buf = self.new_frame(width, height);
        let frame = LayoutFrame::new(width, height);
        let footer_height = if self.api_open { 8 } else { 3 };
        let body_top = frame.y + 1;
        let body_height = frame.height.saturating_sub(1 + footer_height);
        self.draw_footer(
            &mut buf,
            frame.x,
            body_top + body_height,
            frame.width,
            footer_height,
        )?;
        self.present(buf, width, height)
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

    fn draw_header(&self, out: &mut Screen, start_x: u16, width: u16, y: u16) -> Result<()> {
        draw_cell(out, start_x, y, width, "", theme().dim, Some(theme().bg), false)?;
        let mut x = start_x;
        draw_span(
            out,
            &mut x,
            y,
            " plane-tui ",
            theme().ink,
            Some(theme().accent),
            true,
        )?;
        draw_text(
            out,
            &mut x,
            y,
            &format!(" {} │ ", self.client.config.workspace),
            theme().dim,
        )?;
        for (index, project) in self.projects.iter().enumerate() {
            let tab = format!("{}:{} {} ", index + 1, project.identifier, project.name);
            if index == self.active_project {
                draw_span(out, &mut x, y, &tab, theme().ink, Some(theme().paper), true)?;
            } else {
                draw_text(out, &mut x, y, &tab, theme().dim)?;
            }
            draw_text(out, &mut x, y, "· ", theme().line)?;
        }
        let host = self
            .client
            .config
            .base_url
            .replace("https://", "")
            .replace("http://", "");
        let mut right_segments: Vec<(String, Color, bool)> = Vec::new();
        if !self.search.is_empty() {
            right_segments.push((format!("/{}", self.search), theme().accent, false));
        }
        if self.filter != FilterMode::All {
            right_segments.push((format!("f:{}", self.filter.label()), theme().accent, false));
        }
        right_segments.push((format!("sort:{}", self.sort.label()), theme().dim, false));
        // fleet indicator: ⚑N need you (amber) + running count with a spinner
        let fleet_running = self
            .agent_jobs
            .iter()
            .filter(|handle| handle.job.status == jobs::JobStatus::Running)
            .count();
        let fleet_needs = self
            .agent_jobs
            .iter()
            .filter(|handle| FleetBucket::of(handle.job.status) == FleetBucket::NeedsYou)
            .count();
        if fleet_needs > 0 {
            right_segments.push((format!("⚑{fleet_needs}"), theme().amber, true));
        }
        if fleet_running > 0 {
            right_segments.push((
                format!("J{fleet_running} {}", FRAMES[self.frame]),
                theme().green,
                false,
            ));
        } else {
            right_segments.push(("J".to_owned(), theme().dimmer, false));
        }
        let sync_secs = self.project().loaded_at.elapsed().as_secs();
        let (sync_text, sync_color) = if sync_secs < 60 {
            ("⟳now".to_owned(), theme().dimmer)
        } else if sync_secs < 3600 {
            (
                format!("⟳{}m", sync_secs / 60),
                if sync_secs >= 900 { theme().amber } else { theme().dimmer },
            )
        } else {
            (format!("⟳{}h", sync_secs / 3600), theme().amber)
        };
        right_segments.push((sync_text, sync_color, false));
        right_segments.push((host, theme().dim, false));
        right_segments.push(("●".to_owned(), theme().green, false));

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
                        draw_span(out, &mut right_x, y, " ", theme().dimmer, Some(theme().bg), false)?;
                    }
                    draw_span(out, &mut right_x, y, text, *color, Some(theme().bg), *bold)?;
                }
            }
        }
        Ok(())
    }

    fn draw_board(&self, out: &mut Screen, x: u16, y: u16, width: u16, height: u16) -> Result<()> {
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
            draw_cell(out, col_x, y, effective_width, "", theme().dim, Some(theme().bg), false)?;
            let mut header_x = col_x + 1;
            draw_span(
                out,
                &mut header_x,
                y,
                state.glyph(),
                state.color(),
                Some(theme().bg),
                true,
            )?;
            draw_span(out, &mut header_x, y, " ", theme().dim, Some(theme().bg), false)?;
            let wip_limit = self.wip_limit();
            let over_wip = *state == StateKind::Started && wip_limit > 0 && total > wip_limit;
            draw_span(
                out,
                &mut header_x,
                y,
                state.name(),
                if over_wip { theme().red } else { theme().paper },
                Some(theme().bg),
                true,
            )?;
            draw_span(out, &mut header_x, y, " ", theme().dim, Some(theme().bg), false)?;
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
                if over_wip { theme().red } else { theme().dim },
                Some(theme().bg),
                over_wip,
            )?;
            if !shown.is_empty() && effective_width as usize > shown.width() + 1 {
                draw_cell(
                    out,
                    col_x + effective_width.saturating_sub(shown.width() as u16 + 1),
                    y,
                    shown.width() as u16,
                    shown.trim(),
                    theme().dimmer,
                    Some(theme().bg),
                    false,
                )?;
            }
            queue!(
                out,
                MoveTo(col_x, y + 1),
                SetForegroundColor(theme().line),
                SetBackgroundColor(theme().bg),
                Print("─".repeat(effective_width.saturating_sub(1) as usize))
            )?;
            for row in 0..height {
                queue!(
                    out,
                    MoveTo(col_x + effective_width.saturating_sub(1), y + row),
                    SetForegroundColor(theme().line),
                    SetBackgroundColor(theme().bg),
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
                    theme().dim,
                    Some(theme().bg),
                    false,
                )?;
            }
            col_x = col_x.saturating_add(effective_width);
        }
        Ok(())
    }

    fn draw_card(
        &self,
        out: &mut Screen,
        x: u16,
        y: u16,
        width: u16,
        item: &WorkItem,
        selected: bool,
    ) -> Result<()> {
        let fg = if selected { theme().ink } else { theme().paper };
        let marked = if self.marks.contains(&item.key) {
            "✓"
        } else {
            " "
        };
        let border_color = if selected { theme().accent } else { theme().line };
        draw_card_border(out, x, y, width, border_color, Some(theme().cell_bg))?;
        let inner_x = x + 1;
        let inner_width = width.saturating_sub(2);
        draw_cell(
            out,
            inner_x,
            y + 1,
            inner_width,
            "",
            theme().dim,
            Some(theme().cell_bg),
            false,
        )?;
        let mut cursor = inner_x;
        draw_span(out, &mut cursor, y + 1, marked, theme().accent, Some(theme().cell_bg), true)?;
        draw_span(out, &mut cursor, y + 1, " ", theme().dim, Some(theme().cell_bg), false)?;
        draw_span(
            out,
            &mut cursor,
            y + 1,
            &item.key,
            theme().dim,
            Some(theme().cell_bg),
            false,
        )?;
        if let Some((badge, badge_color)) = self.job_badge(&item.key) {
            draw_span(out, &mut cursor, y + 1, " ", theme().dim, Some(theme().cell_bg), false)?;
            draw_span(
                out,
                &mut cursor,
                y + 1,
                badge,
                badge_color,
                Some(theme().cell_bg),
                true,
            )?;
        }
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
                Some(theme().cell_bg),
                true,
            )?;
        }
        let title_bg = if selected {
            Some(theme().accent)
        } else {
            Some(theme().cell_bg)
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
                theme().dimmer,
            ),
        };
        self.draw_card_labels(out, inner_x, y + 4, inner_width, item, &meta, meta_color)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_card_labels(
        &self,
        out: &mut Screen,
        x: u16,
        y: u16,
        width: u16,
        item: &WorkItem,
        age: &str,
        age_color: Color,
    ) -> Result<()> {
        draw_cell(out, x, y, width, "", theme().dim, Some(theme().cell_bg), false)?;
        let age_width = age.width().min(width as usize);
        if age_width < width as usize {
            draw_cell(
                out,
                x + width.saturating_sub(age_width as u16),
                y,
                age_width as u16,
                age,
                age_color,
                Some(theme().cell_bg),
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
                    theme().dim,
                    Some(theme().cell_bg),
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
                Some(theme().cell_bg),
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
                    theme().dim,
                    Some(theme().cell_bg),
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
                draw_cell(out, x, y, label_width, &fallback, theme().dim, Some(theme().cell_bg), false)?;
            }
        }
        Ok(())
    }

    fn draw_list(&self, out: &mut Screen, x: u16, y: u16, width: u16, height: u16) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }

        clear_area(out, x, y, width, height, Some(theme().bg))?;
        let layout = ListLayout::new(width);
        draw_cell(out, x, y, width, "", theme().dimmer, Some(theme().bg), false)?;
        self.draw_list_header(out, x, y, width, layout)?;
        if height > 1 {
            draw_cell(
                out,
                x,
                y + 1,
                width,
                &"─".repeat(width as usize),
                theme().line,
                Some(theme().bg),
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
                theme().dimmer,
                Some(theme().bg),
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
                theme().dimmer,
                Some(theme().bg),
                false,
            )?;
        }
        Ok(())
    }

    fn draw_list_header(
        &self,
        out: &mut Screen,
        x: u16,
        y: u16,
        width: u16,
        layout: ListLayout,
    ) -> Result<()> {
        let bg = Some(theme().bg);
        let mut cursor = x;
        let end = x.saturating_add(width);
        cursor = draw_list_cell(out, cursor, end, y, layout.mark, "", theme().dimmer, bg, false)?;
        cursor = draw_list_cell(out, cursor, end, y, layout.priority, "p", theme().dimmer, bg, false)?;
        cursor = draw_list_cell(out, cursor, end, y, layout.key, "key", theme().dimmer, bg, false)?;
        cursor = draw_list_cell(
            out,
            cursor,
            end,
            y,
            layout.title,
            "title",
            theme().dimmer,
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
            theme().dimmer,
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
            theme().dimmer,
            bg,
            false,
        )?;
        cursor = draw_list_cell_right(out, cursor, end, y, layout.due, "due", theme().dimmer, bg, false)?;
        let _ = draw_list_cell_right(
            out,
            cursor,
            end,
            y,
            layout.updated,
            "updated",
            theme().dimmer,
            bg,
            false,
        )?;
        Ok(())
    }

    fn draw_list_row(
        &self,
        out: &mut Screen,
        x: u16,
        y: u16,
        width: u16,
        layout: ListLayout,
        item: &WorkItem,
        selected: bool,
    ) -> Result<()> {
        let bg = if selected { Some(theme().accent) } else { Some(theme().bg) };
        let selected_fg = theme().ink;
        draw_cell(out, x, y, width, "", selected_fg, bg, false)?;

        let mark_fg = if selected { selected_fg } else { theme().accent };
        let priority_fg = if selected {
            selected_fg
        } else {
            item.priority.color()
        };
        let key_fg = if selected { selected_fg } else { theme().dim };
        let title_fg = if selected { selected_fg } else { theme().paper };
        let state_fg = if selected {
            selected_fg
        } else {
            item.state.color()
        };
        let muted_fg = if selected { selected_fg } else { theme().dimmer };
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
        out: &mut Screen,
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

        let fg = if selected { theme().ink } else { theme().dim };
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
            let label_fg = if selected { theme().ink } else { label.color };
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
        out: &mut Screen,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<()> {
        let content_x = x.saturating_add(1);
        let content_width = width.saturating_sub(1);
        clear_area(out, content_x, y, content_width, height, Some(theme().bg))?;
        for row in 0..height {
            queue!(
                out,
                MoveTo(x, y + row),
                SetForegroundColor(theme().line),
                SetBackgroundColor(theme().bg),
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
                theme().dim,
                None,
                false,
            )?;
            return Ok(());
        };
        let mut row = y;
        draw_cell(out, content_x, row, content_width, "", theme().dim, None, false)?;
        let mut cursor = content_x;
        draw_span(out, &mut cursor, row, &item.key, theme().dim, Some(theme().bg), true)?;
        draw_span(out, &mut cursor, row, " · ", theme().dimmer, Some(theme().bg), false)?;
        draw_span(
            out,
            &mut cursor,
            row,
            &format!("{} {}", item.priority.glyph(), item.priority.as_plane()),
            item.priority.color(),
            Some(theme().bg),
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
                Some(theme().bg),
                true,
            )?;
        }
        row += 1;
        for line in wrap_line(&item.title, width.saturating_sub(3) as usize)
            .into_iter()
            .take(2)
        {
            draw_cell(out, x + 1, row, width - 1, &line, theme().paper, None, true)?;
            row += 1;
        }
        if row < y + height {
            draw_cell(
                out,
                x + 1,
                row,
                width - 1,
                &"─".repeat(width.saturating_sub(2) as usize),
                theme().line,
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
            ("labels", String::new(), theme().text),
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
                        .unwrap_or(theme().text),
                    None => theme().dimmer,
                },
            ),
            (
                "created",
                item.created_at
                    .map(|dt| dt.date_naive().to_string())
                    .unwrap_or_else(|| "unknown".to_owned()),
                theme().text,
            ),
            (
                "updated",
                item.updated_at
                    .map(|dt| format!("{} · {}", dt.date_naive(), time_ago(dt)))
                    .unwrap_or_else(|| "unknown".to_owned()),
                theme().text,
            ),
            (
                "completed",
                item.completed_at
                    .clone()
                    .map(|value| value.chars().take(10).collect::<String>())
                    .unwrap_or_else(|| "none".to_owned()),
                theme().text,
            ),
            (
                "url",
                format!(
                    "{}/{}/browse/{}",
                    self.client.config.base_url, self.client.config.workspace, item.key
                ),
                theme().accent,
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
        draw_cell(out, x + 1, row, width - 1, "description", theme().dim, None, false)?;
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
            draw_cell(out, x + 1, row, width - 1, &line, theme().dim, None, false)?;
            row += 1;
        }
        if !item.actions.is_empty() && row + 1 < y + height {
            row += 1;
            draw_cell(out, x + 1, row, width - 1, "activity", theme().dim, None, false)?;
            row += 1;
            for action in item.actions.iter().take(4) {
                if row >= y + height {
                    break;
                }
                draw_cell(out, x + 1, row, width - 1, action, theme().dim, None, false)?;
                row += 1;
            }
        }
        Ok(())
    }

    fn draw_footer(&self, out: &mut Screen, x: u16, y: u16, width: u16, height: u16) -> Result<()> {
        queue!(
            out,
            MoveTo(x, y),
            SetForegroundColor(theme().line),
            SetBackgroundColor(theme().bg),
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
                theme().dim,
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
                        "PATCH" => theme().amber,
                        "POST" => theme().green,
                        "GET" => theme().accent,
                        _ => theme().text,
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
                    theme().ink,
                    Some(theme().accent),
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
                    MenuMode::Dispatch => {
                        let registry = repo_registry(&self.client.config);
                        let repo_label = registry
                            .get(self.dispatch_repo)
                            .or_else(|| registry.first())
                            .map(|(name, _)| name.as_str())
                            .unwrap_or("?");
                        format!(
                            "dispatch {} → enter {}  1 codex  2 claude  i int:{}  b brief:{}  e stance:{}  s skills:{}  r repo:{}  esc",
                            self.dispatch_item.as_deref().unwrap_or("?"),
                            match self.dispatch_backend {
                                AgentBackend::Codex => "codex",
                                AgentBackend::Claude => "claude",
                            },
                            if self.dispatch_interactive {
                                "on"
                            } else {
                                "off"
                            },
                            if self.dispatch_brief {
                                "fable-5"
                            } else {
                                "env"
                            },
                            if self.dispatch_explore {
                                "explore"
                            } else {
                                "impl"
                            },
                            self.dispatch_skills.len(),
                            repo_label,
                        )
                    }
                    MenuMode::Feedback => {
                        let (key, backend) = self
                            .feedback_job
                            .as_deref()
                            .and_then(|id| self.job_index_by_id(id))
                            .map(|index| {
                                let job = &self.agent_jobs[index].job;
                                (job.item_key.clone(), job.backend.clone())
                            })
                            .unwrap_or_else(|| ("?".to_owned(), "?".to_owned()));
                        format!(
                            "feedback {key} → executor: 1/enter keep ({backend})   2 codex   3 claude · {}   esc cancel",
                            self.client.config.claude_model
                        )
                    }
                    MenuMode::Land => format!(
                        "land {} → m merge into repo branch   P push + PR (gh)   b push only   esc cancel",
                        self.land_job
                            .as_deref()
                            .and_then(|id| self.job_index_by_id(id))
                            .map(|index| self.agent_jobs[index].job.item_key.clone())
                            .unwrap_or_else(|| "?".to_owned())
                    ),
                };
                if menu != MenuMode::Label {
                    let menu_bg = if menu == MenuMode::ConfirmWip {
                        theme().red
                    } else {
                        theme().paper
                    };
                    draw_cell(
                        out,
                        inner_x,
                        row,
                        inner_width,
                        &text,
                        theme().ink,
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
            let hint = if self.jobs_open {
                "j/k select · enter diff · t dive · f feedback · l land · r retry · c cancel · x discard · J/esc board"
            } else {
                "j/k h/l move · enter detail · e edit · a/A agent · m mark · s state · p priority · t label · D done · T triage · v view · J fleet · / search · : cmd · x api · ? keys · q quit"
            };
            draw_cell(out, inner_x, row, inner_width, hint, theme().dimmer, None, false)?;
        }
        Ok(())
    }

    fn draw_command_line(&self, out: &mut Screen, x: u16, y: u16, width: u16) -> Result<()> {
        if width == 0 {
            return Ok(());
        }

        draw_cell(out, x, y, width, "", theme().text, Some(theme().bg), false)?;

        let left = self.command_line_text();
        let left_color = if self.input_mode.is_some() {
            theme().paper
        } else {
            theme().dim
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

            draw_cell(out, x, y, left_width, &left, left_color, Some(theme().bg), false)?;

            let mut cursor = x + width.saturating_sub(right_width);
            if job_width > 0 {
                draw_cell(out, cursor, y, job_width, &job, theme().amber, Some(theme().bg), true)?;
                cursor = cursor.saturating_add(job_width).saturating_add(gap);
            }
            if position_width > 0 && cursor < x + width {
                draw_cell(
                    out,
                    cursor,
                    y,
                    min(position_width, x + width - cursor),
                    &position,
                    theme().dimmer,
                    Some(theme().bg),
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
        draw_cell(out, x, y, left_width, &left, left_color, Some(theme().bg), false)?;
        if position_width > 0 {
            draw_cell(
                out,
                x + width.saturating_sub(position_width),
                y,
                position_width,
                &position,
                theme().dimmer,
                Some(theme().bg),
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
                InputMode::DispatchExtra => "dispatch note (optional, appended to the brief) → ",
                InputMode::FeedbackNote => "feedback for the agent → ",
                InputMode::WorkFolder => "folder for the work session → ",
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

    fn draw_backend_wizard_bar(&self, out: &mut Screen, x: u16, y: u16, width: u16) -> Result<()> {
        let Some(wizard) = self.backend_wizard.as_ref() else {
            return Ok(());
        };
        let end = x.saturating_add(width);
        draw_cell(out, x, y, width, "", theme().dim, Some(theme().bg_raise), false)?;
        let mut cursor = x;
        draw_span_clipped(
            out,
            &mut cursor,
            end,
            y,
            " backend → ",
            theme().ink,
            Some(theme().paper),
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
            theme().dim,
            Some(theme().bg_raise),
            false,
        )?;
        draw_span_clipped(
            out,
            &mut cursor,
            end,
            y,
            "  m model  e effort  enter save  esc cancel",
            theme().dimmer,
            Some(theme().bg_raise),
            false,
        )?;
        Ok(())
    }

    fn draw_backend_wizard_choice(
        &self,
        out: &mut Screen,
        cursor: &mut u16,
        end: u16,
        y: u16,
        label: &str,
        selected: bool,
    ) -> Result<()> {
        draw_span_clipped(out, cursor, end, y, " ", theme().dim, Some(theme().bg_raise), false)?;
        let bg = if selected { theme().accent } else { theme().cell_bg };
        let fg = if selected { theme().ink } else { theme().paper };
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

    fn draw_label_menu_bar(&self, out: &mut Screen, x: u16, y: u16, width: u16) -> Result<()> {
        draw_cell(out, x, y, width, "", theme().dim, Some(theme().bg_raise), false)?;
        let mut cursor = x;
        draw_span(
            out,
            &mut cursor,
            y,
            &format!(" toggle label → {} item ", self.target_keys().len()),
            theme().ink,
            Some(theme().paper),
            true,
        )?;
        for (index, label) in self.project().labels.iter().take(9).enumerate() {
            if cursor.saturating_sub(x) >= width.saturating_sub(14) {
                break;
            }
            draw_span(out, &mut cursor, y, " ", theme().dim, Some(theme().bg_raise), false)?;
            draw_span(
                out,
                &mut cursor,
                y,
                &(index + 1).to_string(),
                theme().accent,
                Some(theme().bg_raise),
                true,
            )?;
            draw_span(out, &mut cursor, y, " ", theme().dim, Some(theme().bg_raise), false)?;
            let text = format!("{}{}", color_marker(label.color), label.name);
            let remaining = width.saturating_sub(cursor.saturating_sub(x)) as usize;
            draw_span(
                out,
                &mut cursor,
                y,
                &truncate(&text, remaining.min(16)),
                label.color,
                Some(theme().bg_raise),
                false,
            )?;
        }
        if cursor.saturating_sub(x) < width.saturating_sub(12) {
            draw_span(out, &mut cursor, y, "  n ", theme().dim, Some(theme().bg_raise), false)?;
            draw_span(
                out,
                &mut cursor,
                y,
                "new label",
                theme().green,
                Some(theme().bg_raise),
                true,
            )?;
        }
        if cursor.saturating_sub(x) < width.saturating_sub(10) {
            draw_span(
                out,
                &mut cursor,
                y,
                "  esc done",
                theme().dim,
                Some(theme().bg_raise),
                false,
            )?;
        }
        Ok(())
    }

    fn draw_keys_overlay(&self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
        draw_help_panel(out, width, height)
    }

    fn draw_notes_overlay(&self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
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

    fn draw_triage_overlay(&self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
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

    fn draw_prompt_overlay(&mut self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
        self.draw_prompt_overlay_inner(out, width, height, true)
    }

    fn draw_prompt_overlay_body(
        &mut self,
        out: &mut Screen,
        width: u16,
        height: u16,
    ) -> Result<()> {
        self.draw_prompt_overlay_inner(out, width, height, false)
    }

    fn draw_prompt_overlay_inner(
        &mut self,
        out: &mut Screen,
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
                theme().text,
                Some(theme().bg),
                false,
            )?;
        }
        draw_cell(
            out,
            x + 3,
            y + box_height.saturating_sub(3),
            box_width.saturating_sub(6),
            &hint,
            theme().dim,
            Some(theme().bg),
            false,
        )?;
        Ok(())
    }

    fn draw_detail_overlay(&mut self, out: &mut Screen, width: u16, height: u16) -> Result<()> {
        self.draw_detail_overlay_inner(out, width, height, true)
    }

    fn draw_detail_overlay_body(
        &mut self,
        out: &mut Screen,
        width: u16,
        height: u16,
    ) -> Result<()> {
        self.draw_detail_overlay_inner(out, width, height, false)
    }

    fn draw_detail_overlay_inner(
        &mut self,
        out: &mut Screen,
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
            lines.push((line, theme().paper, true));
        }
        lines.push((String::new(), theme().text, false));
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
        lines.push((format!("labels    {labels}"), theme().text, false));
        let (due_text, due_color) = match item.due.as_deref() {
            Some(due) => {
                let (_, color) = list_due(Some(due));
                (due.to_owned(), color)
            }
            None => ("none".to_owned(), theme().dimmer),
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
            theme().text,
            false,
        ));
        lines.push((
            format!(
                "url       {}/{}/browse/{}",
                self.client.config.base_url, self.client.config.workspace, item.key
            ),
            theme().accent,
            false,
        ));
        lines.push((String::new(), theme().text, false));
        lines.push(("description".to_owned(), theme().dim, false));
        let description = if item.description.trim().is_empty() {
            "(no description)".to_owned()
        } else {
            item.description.clone()
        };
        for raw_line in description.lines() {
            for line in wrap_line(raw_line, content_width) {
                lines.push((line, theme().text, false));
            }
        }
        lines.push((String::new(), theme().text, false));
        lines.push((format!("comments ({})", detail.comments.len()), theme().dim, false));
        if detail.comments.is_empty() {
            lines.push(("(no comments)".to_owned(), theme().dimmer, false));
        }
        for (when, text) in &detail.comments {
            lines.push((format!("· {when}"), theme().accent, false));
            for raw_line in text.lines() {
                for line in wrap_line(raw_line, content_width.saturating_sub(2)) {
                    lines.push((format!("  {line}"), theme().text, false));
                }
            }
            lines.push((String::new(), theme().text, false));
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
                .unwrap_or(("", theme().text, false));
            draw_cell(
                out,
                x + 3,
                y + 2 + offset as u16,
                box_width.saturating_sub(6),
                line,
                color,
                Some(theme().bg),
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
            theme().dim,
            Some(theme().bg),
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
    set_active_theme(theme_by_name(&config.theme_name).unwrap_or(THEME_MIDNIGHT));
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
        d if d < 0 => Some((format!("{}d over", -d), theme().red)),
        0 => Some(("due today".to_owned(), theme().red)),
        1 => Some(("due tom".to_owned(), theme().amber)),
        2..=3 => Some((format!("due {}", due.get(5..).unwrap_or(due)), theme().amber)),
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

fn theme_config_path() -> Result<PathBuf> {
    Ok(plane_tui_data_dir()?.join("theme"))
}

/// The color scheme name persisted by a previous `:theme`/`C` choice, if any.
fn saved_theme_name() -> Option<String> {
    let raw = fs::read_to_string(theme_config_path().ok()?).ok()?;
    let name = raw.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Remember the chosen scheme so it survives restarts. Best-effort — a failure
/// to write just means the choice isn't sticky.
fn save_theme_name(name: &str) {
    let Ok(path) = theme_config_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, name);
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
        return ("-".to_owned(), theme().dimmer);
    };
    let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") else {
        return (value.to_owned(), theme().text);
    };
    let days = date
        .signed_duration_since(Local::now().date_naive())
        .num_days();
    let text = match days {
        0 => "today".to_owned(),
        1 => "tom".to_owned(),
        _ => value.get(5..).unwrap_or(value).to_owned(),
    };
    let color = if days <= 3 { theme().red } else { theme().text };
    (text, color)
}

fn fleet_glyph(status: jobs::JobStatus, stalled: bool, frame: usize) -> &'static str {
    if stalled {
        return "\u{26a0}";
    }
    match status {
        jobs::JobStatus::Briefing => "\u{270e}",
        jobs::JobStatus::Queued => "\u{25cf}",
        jobs::JobStatus::Running => FRAMES[frame],
        jobs::JobStatus::Review => "\u{2691}",
        jobs::JobStatus::Question => "?",
        jobs::JobStatus::Failed | jobs::JobStatus::Orphaned => "\u{2717}",
        jobs::JobStatus::Landed => "\u{2713}",
        jobs::JobStatus::Discarded => "\u{b7}",
    }
}

fn fleet_color(status: jobs::JobStatus, stalled: bool) -> Color {
    if stalled {
        return theme().amber;
    }
    match status {
        jobs::JobStatus::Running | jobs::JobStatus::Landed => theme().green,
        jobs::JobStatus::Review | jobs::JobStatus::Question | jobs::JobStatus::Briefing => theme().amber,
        jobs::JobStatus::Failed | jobs::JobStatus::Orphaned => theme().red,
        jobs::JobStatus::Queued | jobs::JobStatus::Discarded => theme().dimmer,
    }
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

// ── Diffing back-buffer renderer ───────────────────────────────────────────
//
// The draw_* functions all emit crossterm commands into a `&mut dyn io::Write`.
// Instead of streaming those straight to the terminal (a full repaint every
// frame — the source of the flicker/lag), we point them at a `Screen`, which is
// a tiny terminal emulator: it parses the exact escape vocabulary crossterm
// produces (CUP, SGR 38/48/39/49/0/1, and OSC 8 hyperlinks) into a cell grid.
// At the end of a frame we diff that grid against what's currently on the
// terminal and emit only the cells that actually changed. Unchanged regions —
// and closed overlays under `force_clear` — cost nothing, so there is no flash
// and the byte volume per keystroke collapses.

/// A foreground/background colour, stored as the exact SGR form crossterm emits
/// so it round-trips byte-for-byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ink {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl Ink {
    fn write_fg(self, out: &mut String) {
        match self {
            Ink::Default => out.push_str("\x1b[39m"),
            Ink::Idx(n) => {
                let _ = write!(out, "\x1b[38;5;{n}m");
            }
            Ink::Rgb(r, g, b) => {
                let _ = write!(out, "\x1b[38;2;{r};{g};{b}m");
            }
        }
    }
    fn write_bg(self, out: &mut String) {
        match self {
            Ink::Default => out.push_str("\x1b[49m"),
            Ink::Idx(n) => {
                let _ = write!(out, "\x1b[48;5;{n}m");
            }
            Ink::Rgb(r, g, b) => {
                let _ = write!(out, "\x1b[48;2;{r};{g};{b}m");
            }
        }
    }
}

const fn ink_of(c: Color) -> Ink {
    match c {
        Color::Rgb { r, g, b } => Ink::Rgb(r, g, b),
        _ => Ink::Default,
    }
}
#[derive(Clone, PartialEq, Debug)]
struct Cell {
    ch: char,
    fg: Ink,
    bg: Ink,
    bold: bool,
    link: Option<Rc<str>>,
    /// right half of a wide (double-width) glyph; the lead cell paints it.
    cont: bool,
}

impl Cell {
    fn blank() -> Self {
        Cell {
            ch: ' ',
            fg: Ink::Default,
            bg: ink_of(theme().bg),
            bold: false,
            link: None,
            cont: false,
        }
    }
}

#[derive(PartialEq, Eq)]
enum ParseState {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// A cell grid that consumes crossterm's escape stream (via `io::Write`) and can
/// diff itself against a previous frame.
struct Screen {
    w: u16,
    h: u16,
    cells: Vec<Cell>,
    // parser cursor + pen
    cx: u16,
    cy: u16,
    fg: Ink,
    bg: Ink,
    bold: bool,
    link: Option<Rc<str>>,
    // stream-parser scratch
    state: ParseState,
    params: Vec<u16>,
    cur_param: Option<u16>,
    priv_mode: bool,
    osc: Vec<u8>,
    ground: Vec<u8>,
}

impl Default for Screen {
    fn default() -> Self {
        Screen::blank(0, 0)
    }
}

impl Screen {
    fn blank(w: u16, h: u16) -> Self {
        Screen {
            w,
            h,
            cells: vec![Cell::blank(); w as usize * h as usize],
            cx: 0,
            cy: 0,
            fg: Ink::Default,
            bg: Ink::Default,
            bold: false,
            link: None,
            state: ParseState::Ground,
            params: Vec::new(),
            cur_param: None,
            priv_mode: false,
            osc: Vec::new(),
            ground: Vec::new(),
        }
    }

    /// Reset the parser cursor/pen at the start of a frame. The grid content is
    /// left intact so partial redraws (footer-only) keep the rest of the frame.
    fn begin_frame(&mut self) {
        self.cx = 0;
        self.cy = 0;
        self.fg = Ink::Default;
        self.bg = Ink::Default;
        self.bold = false;
        self.link = None;
        self.state = ParseState::Ground;
        self.params.clear();
        self.cur_param = None;
        self.priv_mode = false;
        self.osc.clear();
        self.ground.clear();
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.w as usize + x as usize
    }

    fn flush_ground(&mut self) {
        if self.ground.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.ground).into_owned();
        self.ground.clear();
        for ch in text.chars() {
            self.put_char(ch);
        }
    }

    fn put_char(&mut self, ch: char) {
        let cw = ch.width().unwrap_or(0);
        if cw == 0 {
            return; // skip zero-width / control chars
        }
        if self.cy < self.h && self.cx < self.w {
            let i = self.idx(self.cx, self.cy);
            self.cells[i] = Cell {
                ch,
                fg: self.fg,
                bg: self.bg,
                bold: self.bold,
                link: self.link.clone(),
                cont: false,
            };
            if cw == 2 && self.cx + 1 < self.w {
                let j = i + 1;
                self.cells[j] = Cell {
                    ch: ' ',
                    fg: self.fg,
                    bg: self.bg,
                    bold: self.bold,
                    link: self.link.clone(),
                    cont: true,
                };
            }
        }
        self.cx = self.cx.saturating_add(cw as u16);
    }

    fn apply_sgr(&mut self) {
        if self.params.is_empty() {
            self.params.push(0);
        }
        let mut i = 0;
        while i < self.params.len() {
            match self.params[i] {
                0 => {
                    self.fg = Ink::Default;
                    self.bg = Ink::Default;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                39 => self.fg = Ink::Default,
                49 => self.bg = Ink::Default,
                38 | 48 => {
                    let is_fg = self.params[i] == 38;
                    match self.params.get(i + 1) {
                        Some(2) => {
                            let r = self.params.get(i + 2).copied().unwrap_or(0) as u8;
                            let g = self.params.get(i + 3).copied().unwrap_or(0) as u8;
                            let b = self.params.get(i + 4).copied().unwrap_or(0) as u8;
                            let ink = Ink::Rgb(r, g, b);
                            if is_fg {
                                self.fg = ink;
                            } else {
                                self.bg = ink;
                            }
                            i += 4;
                        }
                        Some(5) => {
                            let n = self.params.get(i + 2).copied().unwrap_or(0) as u8;
                            let ink = Ink::Idx(n);
                            if is_fg {
                                self.fg = ink;
                            } else {
                                self.bg = ink;
                            }
                            i += 2;
                        }
                        _ => {}
                    }
                }
                p @ 30..=37 => self.fg = Ink::Idx((p - 30) as u8),
                p @ 90..=97 => self.fg = Ink::Idx((p - 90 + 8) as u8),
                p @ 40..=47 => self.bg = Ink::Idx((p - 40) as u8),
                p @ 100..=107 => self.bg = Ink::Idx((p - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    fn handle_csi(&mut self, final_byte: u8) {
        match final_byte {
            b'H' | b'f' => {
                let row = self.params.first().copied().unwrap_or(1).max(1) - 1;
                let col = self.params.get(1).copied().unwrap_or(1).max(1) - 1;
                self.cy = row;
                self.cx = col;
            }
            b'm' if !self.priv_mode => self.apply_sgr(),
            b'J' if self.params.first().copied().unwrap_or(0) == 2 => {
                for cell in &mut self.cells {
                    *cell = Cell {
                        ch: ' ',
                        fg: Ink::Default,
                        bg: self.bg,
                        bold: false,
                        link: None,
                        cont: false,
                    };
                }
            }
            _ => {} // private-mode toggles (?25/?2026) and anything else: ignore
        }
    }

    fn handle_osc(&mut self) {
        // Only OSC 8 hyperlinks are emitted: "8;<params>;<uri>".
        let s = String::from_utf8_lossy(&self.osc).into_owned();
        self.osc.clear();
        if let Some(rest) = s.strip_prefix("8;") {
            // rest = "<params>;<uri>"; params are always empty here.
            if let Some(uri) = rest.splitn(2, ';').nth(1) {
                self.link = if uri.is_empty() {
                    None
                } else {
                    Some(Rc::from(uri))
                };
            }
        }
    }

    fn feed(&mut self, byte: u8) {
        match self.state {
            ParseState::Ground => {
                if byte == 0x1b {
                    self.flush_ground();
                    self.state = ParseState::Esc;
                } else {
                    self.ground.push(byte);
                }
            }
            ParseState::Esc => match byte {
                b'[' => {
                    self.params.clear();
                    self.cur_param = None;
                    self.priv_mode = false;
                    self.state = ParseState::Csi;
                }
                b']' => {
                    self.osc.clear();
                    self.state = ParseState::Osc;
                }
                _ => self.state = ParseState::Ground,
            },
            ParseState::Csi => {
                if byte == b'?' {
                    self.priv_mode = true;
                } else if byte.is_ascii_digit() {
                    self.cur_param =
                        Some(self.cur_param.unwrap_or(0).saturating_mul(10) + (byte - b'0') as u16);
                } else if byte == b';' {
                    let p = self.cur_param.take().unwrap_or(0);
                    self.params.push(p);
                } else {
                    if let Some(p) = self.cur_param.take() {
                        self.params.push(p);
                    }
                    self.handle_csi(byte);
                    self.state = ParseState::Ground;
                }
            }
            ParseState::Osc => match byte {
                0x07 => {
                    self.handle_osc();
                    self.state = ParseState::Ground;
                }
                0x1b => self.state = ParseState::OscEsc,
                _ => self.osc.push(byte),
            },
            ParseState::OscEsc => {
                if byte == b'\\' {
                    self.handle_osc();
                    self.state = ParseState::Ground;
                } else {
                    self.osc.push(0x1b);
                    self.osc.push(byte);
                    self.state = ParseState::Osc;
                }
            }
        }
    }

    /// Diff this freshly-drawn frame against `prev` (what's on the terminal),
    /// emit only the changed cells, and update `prev` to match.
    fn flush_into(&mut self, prev: &mut Screen, out: &mut dyn io::Write) -> Result<()> {
        self.flush_ground();
        let mut o = String::new();
        o.push_str("\x1b[0m");
        let mut cf = Ink::Default;
        let mut cb = Ink::Default;
        let mut cbold = false;
        let mut clink: Option<Rc<str>> = None;
        for y in 0..self.h {
            let mut pen: Option<u16> = None;
            let mut x = 0u16;
            while x < self.w {
                let i = self.idx(x, y);
                let cell = &self.cells[i];
                if cell.cont {
                    // painted by the lead; keep prev in sync and move on
                    if prev.cells[i] != *cell {
                        prev.cells[i] = cell.clone();
                    }
                    x += 1;
                    continue;
                }
                let wide = (x + 1 < self.w) && self.cells[i + 1].cont;
                let cw = if wide { 2 } else { 1 };
                let changed =
                    prev.cells[i] != *cell || (wide && prev.cells[i + 1] != self.cells[i + 1]);
                if !changed {
                    x += cw;
                    continue;
                }
                if pen != Some(x) {
                    let _ = write!(o, "\x1b[{};{}H", y + 1, x + 1);
                }
                // style delta
                if cbold && !cell.bold {
                    o.push_str("\x1b[0m");
                    cf = Ink::Default;
                    cb = Ink::Default;
                    cbold = false;
                } else if !cbold && cell.bold {
                    o.push_str("\x1b[1m");
                    cbold = true;
                }
                if cf != cell.fg {
                    cell.fg.write_fg(&mut o);
                    cf = cell.fg;
                }
                if cb != cell.bg {
                    cell.bg.write_bg(&mut o);
                    cb = cell.bg;
                }
                if clink != cell.link {
                    match &cell.link {
                        Some(uri) => {
                            let _ = write!(o, "\x1b]8;;{uri}\x1b\\");
                        }
                        None => o.push_str("\x1b]8;;\x1b\\"),
                    }
                    clink = cell.link.clone();
                }
                o.push(cell.ch);
                pen = Some(x + cw);
                prev.cells[i] = cell.clone();
                if wide {
                    prev.cells[i + 1] = self.cells[i + 1].clone();
                }
                x += cw;
            }
        }
        if clink.is_some() {
            o.push_str("\x1b]8;;\x1b\\");
        }
        o.push_str("\x1b[0m");
        out.write_all(o.as_bytes())?;
        Ok(())
    }
}

impl io::Write for Screen {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &byte in buf {
            self.feed(byte);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn draw_text(out: &mut Screen, x: &mut u16, y: u16, text: &str, fg: Color) -> Result<()> {
    queue!(
        out,
        MoveTo(*x, y),
        SetForegroundColor(fg),
        SetBackgroundColor(theme().bg),
        Print(text),
        ResetColor
    )?;
    *x = x.saturating_add(text.width() as u16);
    Ok(())
}

fn draw_link_field(
    out: &mut Screen,
    x: u16,
    y: u16,
    width: u16,
    url: &str,
    value_color: Color,
) -> Result<()> {
    draw_cell(out, x, y, width, "", theme().dim, None, false)?;
    let mut cursor = x;
    draw_span(
        out,
        &mut cursor,
        y,
        &format!("{:<9}", "url"),
        theme().dimmer,
        Some(theme().bg),
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
            SetBackgroundColor(theme().bg),
            Print(format!("\x1b]8;;{url}\x1b\\{display}\x1b]8;;\x1b\\")),
            ResetColor
        )?;
    }
    Ok(())
}

fn draw_field_line(
    out: &mut Screen,
    x: u16,
    y: u16,
    width: u16,
    name: &str,
    value: &str,
    value_color: Color,
) -> Result<()> {
    draw_cell(out, x, y, width, "", theme().dim, None, false)?;
    let mut cursor = x;
    draw_span(
        out,
        &mut cursor,
        y,
        &format!("{name:<9}"),
        theme().dimmer,
        Some(theme().bg),
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
            Some(theme().bg),
            false,
        )?;
    }
    Ok(())
}

fn draw_label_field(
    out: &mut Screen,
    x: u16,
    y: u16,
    width: u16,
    project: &Project,
    item: &WorkItem,
) -> Result<()> {
    draw_cell(out, x, y, width, "", theme().dim, None, false)?;
    let mut cursor = x;
    draw_span(out, &mut cursor, y, "labels   ", theme().dimmer, Some(theme().bg), false)?;
    if item.label_ids.is_empty() {
        draw_span(
            out,
            &mut cursor,
            y,
            "none · t to add",
            theme().dimmer,
            Some(theme().bg),
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
            draw_span(out, &mut cursor, y, " ", theme().dim, Some(theme().bg), false)?;
        }
        let remaining = width.saturating_sub(cursor.saturating_sub(x));
        let text = format!("{}{}", color_marker(label.color), label.name);
        draw_span(
            out,
            &mut cursor,
            y,
            &truncate(&text, remaining as usize),
            label.color,
            Some(theme().bg),
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
            theme().text,
            Some(theme().bg),
            false,
        )?;
    }
    Ok(())
}

fn draw_outer_frame(out: &mut Screen, frame: LayoutFrame) -> Result<()> {
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
        SetForegroundColor(theme().line),
        SetBackgroundColor(theme().bg),
        Print("┌"),
        Print("─".repeat(width.saturating_sub(2) as usize)),
        Print("┐")
    )?;
    for row in 1..height.saturating_sub(1) {
        queue!(
            out,
            MoveTo(left, top + row),
            SetForegroundColor(theme().line),
            SetBackgroundColor(theme().bg),
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
    out: &mut Screen,
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
    out: &mut Screen,
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
    out: &mut Screen,
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
    out: &mut Screen,
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
    queue!(out, SetBackgroundColor(bg.unwrap_or(theme().bg)))?;
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
    out: &mut Screen,
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
    queue!(out, SetBackgroundColor(bg.unwrap_or(theme().bg)))?;
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
    out: &mut Screen,
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
    out: &mut Screen,
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
    out: &mut Screen,
    x: u16,
    y: u16,
    width: u16,
    color: Color,
    bg: Option<Color>,
) -> Result<()> {
    if width < 2 {
        return Ok(());
    }
    let bg = bg.unwrap_or(theme().bg);
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

fn draw_help_panel(out: &mut Screen, width: u16, height: u16) -> Result<()> {
    let box_width = min(width.saturating_sub(8), 112);
    let box_height = min(height.saturating_sub(6), 30);
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
        "d",
        "dispatch agent → enter default · 1/2 backend · i interactive · b brief · e explore · s skills · r repo",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "w",
        "work session · pick a folder → interactive agent there, latest prompt on clipboard",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "J",
        "fleet · enter diff · t deep dive · f feedback · l land · c/r/x · w sessions too",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "card badge",
        "⚑ agent working (green) / needs review (amber) · ? asked a question",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "",
        "✗ failed · ● queued · ✎ writing brief · ⚠ quiet too long — J to act",
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
        "C",
        "cycle color scheme (midnight · gruvbox · daylight) · also :theme <name>",
    )?;
    row += 1;
    draw_shortcut_row(
        out,
        left,
        value_x,
        row,
        "/  :  f  S",
        "search · command (:new, :project, :backend, :repos, :theme) · filter · sort",
    )?;
    row += 1;
    draw_shortcut_row(out, left, value_x, row, "? / q / esc", "close this panel")?;

    Ok(())
}

fn draw_modal_shell(
    out: &mut Screen,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
) -> Result<()> {
    for row in 0..height {
        draw_cell(out, x, y + row, width, "", theme().text, Some(theme().bg), false)?;
    }
    draw_cell(
        out,
        x,
        y,
        width,
        &format!(" {title}"),
        theme().ink,
        Some(theme().paper),
        true,
    )?;
    queue!(
        out,
        MoveTo(x, y + 1),
        SetForegroundColor(theme().paper),
        SetBackgroundColor(theme().bg),
        Print("│"),
        MoveTo(x + width.saturating_sub(1), y + 1),
        Print("│")
    )?;
    for row in 2..height.saturating_sub(2) {
        queue!(
            out,
            MoveTo(x, y + row),
            SetForegroundColor(theme().paper),
            SetBackgroundColor(theme().bg),
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
        theme().dim,
        Some(theme().bg),
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
            Some(theme().paper),
            false,
        )?;
    }
    draw_cell(
        out,
        x,
        y + height.saturating_sub(1),
        width,
        "",
        theme().paper,
        Some(theme().bg),
        false,
    )?;
    queue!(out, ResetColor)?;
    Ok(())
}

fn draw_help_section(out: &mut Screen, x: u16, y: u16, title: &str) -> Result<()> {
    draw_cell(out, x, y, 28, title, theme().dim, Some(theme().bg), false)
}

fn draw_shortcut_row(
    out: &mut Screen,
    x: u16,
    value_x: u16,
    y: u16,
    keys: &str,
    description: &str,
) -> Result<()> {
    let mut cursor = x;
    draw_span(out, &mut cursor, y, keys, theme().accent, Some(theme().bg), true)?;
    cursor = value_x;
    draw_span(out, &mut cursor, y, description, theme().dim, Some(theme().bg), false)?;
    Ok(())
}

fn draw_overlay(out: &mut Screen, width: u16, height: u16, lines: &[&str]) -> Result<()> {
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

#[cfg(test)]
mod app_tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            base_url: "https://plane.invalid".to_owned(),
            api_key: "test".to_owned(),
            workspace: "test".to_owned(),
            wanted_projects: Vec::new(),
            per_page: 100,
            check_api: false,
            agent_backend: AgentBackend::Codex,
            codex_bin: "codex".to_owned(),
            claude_bin: "claude".to_owned(),
            claude_model: "claude-fable-5".to_owned(),
            claude_effort: "high".to_owned(),
            repo_dir: None,
            context_file: None,
            auto_refresh_mins: 5,
            wip_limit: 2,
            theme_name: THEME_MIDNIGHT.name.to_owned(),
        }
    }

    fn test_item(sequence_id: i64, state: StateKind, priority: Priority) -> WorkItem {
        WorkItem {
            id: format!("item-{sequence_id}"),
            key: format!("TM-{sequence_id}"),
            sequence_id,
            title: format!("item {sequence_id}"),
            state_id: state.slug().to_owned(),
            state,
            priority,
            labels: Vec::new(),
            label_ids: Vec::new(),
            due: None,
            created_at: None,
            updated_at: None,
            completed_at: None,
            description: String::new(),
            actions: Vec::new(),
        }
    }

    fn test_app(items: Vec<WorkItem>) -> App {
        App {
            client: PlaneClient::new(test_config()),
            projects: vec![Project {
                id: "project".to_owned(),
                name: "Project".to_owned(),
                identifier: "TM".to_owned(),
                states: vec![
                    State {
                        id: StateKind::Backlog.slug().to_owned(),
                        name: StateKind::Backlog.name().to_owned(),
                        kind: StateKind::Backlog,
                    },
                    State {
                        id: StateKind::Todo.slug().to_owned(),
                        name: StateKind::Todo.name().to_owned(),
                        kind: StateKind::Todo,
                    },
                    State {
                        id: StateKind::Started.slug().to_owned(),
                        name: StateKind::Started.name().to_owned(),
                        kind: StateKind::Started,
                    },
                    State {
                        id: StateKind::Done.slug().to_owned(),
                        name: StateKind::Done.name().to_owned(),
                        kind: StateKind::Done,
                    },
                ],
                labels: Vec::new(),
                items,
                loaded_at: Instant::now(),
            }],
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
            api_log: Vec::new(),
            status: String::new(),
            busy: None,
            last_g: None,
            frame: 0,
            should_quit: false,
            screen: Screen::default(),
            force_clear: false,
            agent_jobs: Vec::new(),
            jobs_open: false,
            jobs_sel_id: None,
            dispatch_item: None,
            dispatch_backend: AgentBackend::Codex,
            dispatch_interactive: true,
            dispatch_brief: false,
            dispatch_explore: false,
            dispatch_repo: 0,
            repo_wizard: None,
            repo_wizard_sel: 0,
            work_wizard: None,
            work_item: None,
            work_sessions: Vec::new(),
            work_sessions_at: None,
            skill_wizard: None,
            skill_wizard_sel: 0,
            dispatch_skills: Vec::new(),
            feedback_job: None,
            feedback_backend: None,
            land_job: None,
            post_results: Vec::new(),
        }
    }

    #[test]
    fn select_item_by_key_moves_board_cursor_to_item() {
        let mut app = test_app(vec![
            test_item(2, StateKind::Todo, Priority::None),
            test_item(1, StateKind::Todo, Priority::None),
            test_item(3, StateKind::Started, Priority::None),
        ]);
        app.sort = SortMode::Key;

        assert!(app.select_item_by_key("TM-1"));

        assert_eq!(app.column, 1);
        assert_eq!(app.row, 1);
        assert_eq!(app.current_item().unwrap().key, "TM-1");
        assert!(app.force_clear);
    }

    #[test]
    fn select_item_by_key_moves_list_cursor_to_item() {
        let mut app = test_app(vec![
            test_item(3, StateKind::Backlog, Priority::None),
            test_item(2, StateKind::Todo, Priority::None),
            test_item(1, StateKind::Started, Priority::None),
        ]);
        app.view = ViewMode::List;
        app.sort = SortMode::Key;

        assert!(app.select_item_by_key("TM-1"));

        assert_eq!(app.cursor, 2);
        assert_eq!(app.current_item().unwrap().key, "TM-1");
    }

    #[test]
    fn select_item_by_key_reveals_item_hidden_by_filter() {
        let mut app = test_app(vec![
            test_item(2, StateKind::Todo, Priority::High),
            test_item(1, StateKind::Todo, Priority::None),
        ]);
        app.view = ViewMode::List;
        app.sort = SortMode::Key;
        app.filter = FilterMode::Fire;
        app.search = "does-not-match".to_owned();

        assert!(app.select_item_by_key("TM-1"));

        assert_eq!(app.filter, FilterMode::All);
        assert!(app.search.is_empty());
        assert_eq!(app.current_item().unwrap().key, "TM-1");
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn theme_registry_is_well_formed() {
        // The default (first) scheme is midnight, matching the historical palette.
        assert_eq!(THEMES[0].name, THEME_MIDNIGHT.name);
        assert_eq!(THEME_MIDNIGHT.name, "midnight");
        assert!(THEMES.len() >= 2, "expected at least one alternate scheme");

        // Names are unique so lookup and cycling are unambiguous.
        for (i, a) in THEMES.iter().enumerate() {
            for b in &THEMES[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate theme name {}", a.name);
            }
        }
    }

    #[test]
    fn theme_lookup_is_case_insensitive() {
        assert_eq!(theme_by_name("GRUVBOX").map(|t| t.name), Some("gruvbox"));
        assert_eq!(theme_by_name("  daylight ").map(|t| t.name), Some("daylight"));
        assert!(theme_by_name("nope").is_none());
    }

    #[test]
    fn next_theme_cycles_and_wraps() {
        // Walking `next_theme` from the default visits every scheme then wraps.
        let mut name = THEMES[0].name;
        let mut seen = vec![name];
        for _ in 1..THEMES.len() {
            name = next_theme(name).name;
            seen.push(name);
        }
        for scheme in THEMES {
            assert!(seen.contains(&scheme.name), "cycle skipped {}", scheme.name);
        }
        assert_eq!(next_theme(name).name, THEMES[0].name, "cycle should wrap");
        // An unknown name falls back to the first scheme's successor.
        assert_eq!(next_theme("bogus").name, THEMES[1 % THEMES.len()].name);
    }

    #[test]
    fn set_active_theme_swaps_the_palette() {
        set_active_theme(THEME_DAYLIGHT);
        assert_eq!(theme().name, "daylight");
        assert_eq!(theme().bg, THEME_DAYLIGHT.bg);
        // Restore the default so other tests on this thread see midnight.
        set_active_theme(THEME_MIDNIGHT);
        assert_eq!(theme().bg, THEME_MIDNIGHT.bg);
    }

    #[test]
    fn active_theme_flows_into_rendered_cells() {
        // End-to-end: the same draw call paints different inks under different
        // schemes, proving the palette reaches the renderer (not just the const).
        let ink = |scheme: Theme| {
            set_active_theme(scheme);
            let mut s = Screen::blank(8, 1);
            draw_cell(&mut s, 0, 0, 8, "hi", theme().paper, Some(theme().bg), false).unwrap();
            let cell = &s.cells[s.idx(0, 0)];
            (cell.fg, cell.bg)
        };
        let midnight = ink(THEME_MIDNIGHT);
        let daylight = ink(THEME_DAYLIGHT);
        assert_eq!(midnight, (ink_of(THEME_MIDNIGHT.paper), ink_of(THEME_MIDNIGHT.bg)));
        assert_eq!(daylight, (ink_of(THEME_DAYLIGHT.paper), ink_of(THEME_DAYLIGHT.bg)));
        assert_ne!(midnight, daylight);
        set_active_theme(THEME_MIDNIGHT);
    }

    #[test]
    fn work_session_names_include_the_item_title() {
        assert_eq!(
            work_session_name("TM-201", "Upload retry loop (phase 2)"),
            "pti-work-tm-201--upload-retry-loop-phase-2"
        );
        assert_eq!(
            work_session_item_key("pti-work-tm-201--upload-retry-loop-phase-2").as_deref(),
            Some("TM-201")
        );
    }

    #[test]
    fn work_session_names_use_all_safe_space_without_splitting_unicode() {
        let name = work_session_name("TM-201", &"é".repeat(200));

        assert!(name.len() <= WORK_SESSION_NAME_MAX_BYTES);
        assert!(name.starts_with("pti-work-tm-201--"));
        assert!(name.is_char_boundary(name.len()));
    }

    #[test]
    fn work_session_matching_supports_legacy_names_without_key_prefix_collisions() {
        assert!(work_session_matches_item("pti-work-tm-1", "TM-1"));
        assert!(work_session_matches_item(
            "pti-work-tm-1--short-title",
            "TM-1"
        ));
        assert!(!work_session_matches_item(
            "pti-work-tm-10--short-title",
            "TM-1"
        ));
    }

    fn grid_text(s: &Screen) -> Vec<String> {
        (0..s.h)
            .map(|y| {
                (0..s.w)
                    .map(|x| {
                        let c = &s.cells[s.idx(x, y)];
                        if c.cont { '\u{0}' } else { c.ch }
                    })
                    .collect()
            })
            .collect()
    }

    // Replay a flushed stream onto a blank terminal and confirm it reproduces
    // the source frame exactly — this is the invariant that guarantees the diff
    // output is faithful to what the draw_* functions intended.
    fn replay(src: &mut Screen) -> Screen {
        let mut prev = Screen::blank(src.w, src.h);
        let mut bytes: Vec<u8> = Vec::new();
        src.flush_into(&mut prev, &mut bytes).unwrap();
        let mut fresh = Screen::blank(src.w, src.h);
        fresh.write_all(&bytes).unwrap();
        fresh.flush_ground();
        fresh
    }

    #[test]
    fn parses_move_color_print() {
        let mut s = Screen::blank(20, 3);
        draw_cell(&mut s, 0, 0, 20, "hello", theme().paper, Some(theme().bg), false).unwrap();
        let s = replay(&mut s);
        assert_eq!(grid_text(&s)[0], format!("{:<20}", "hello"));
        let c = &s.cells[s.idx(0, 0)];
        assert_eq!(c.ch, 'h');
    }

    #[test]
    fn wide_glyph_marks_continuation() {
        let mut s = Screen::blank(10, 1);
        let mut x = 0;
        // a double-width glyph
        draw_span(&mut s, &mut x, 0, "⣾", theme().accent, Some(theme().bg), false).unwrap();
        s.flush_ground();
        assert_eq!(s.cells[s.idx(0, 0)].ch, '⣾');
        assert!(!s.cells[s.idx(0, 0)].cont);
        // the box glyph ⣾ is width 1 in this app's font metrics; ensure at least
        // that placement + advance are consistent with unicode width.
        let w = "⣾".width() as u16;
        assert_eq!(x, w);
    }

    #[test]
    fn osc8_link_round_trips() {
        let mut s = Screen::blank(40, 1);
        draw_link_field(&mut s, 0, 0, 40, "https://example.com/x", theme().accent).unwrap();
        s.flush_ground();
        // the url cells should carry a link annotation
        let has_link = (0..s.w).any(|x| s.cells[s.idx(x, 0)].link.is_some());
        assert!(has_link, "expected an OSC 8 link somewhere on the row");
        let fresh = replay(&mut s);
        assert_eq!(grid_text(&fresh), grid_text(&s));
        assert!((0..fresh.w).any(|x| fresh.cells[fresh.idx(x, 0)].link.is_some()));
    }

    #[test]
    fn flush_reproduces_frame() {
        let mut s = Screen::blank(24, 4);
        draw_cell(&mut s, 0, 0, 24, "top line", theme().paper, Some(theme().bg), true).unwrap();
        let mut x = 1;
        draw_span(&mut s, &mut x, 1, "mixed", theme().red, Some(theme().cell_bg), false).unwrap();
        draw_span(&mut s, &mut x, 1, " tail", theme().green, None, true).unwrap();
        draw_cell_right(&mut s, 0, 2, 24, "right", theme().dim, Some(theme().bg), false).unwrap();
        s.flush_ground();
        let fresh = replay(&mut s);
        assert_eq!(grid_text(&fresh), grid_text(&s));
        for i in 0..s.cells.len() {
            assert_eq!(fresh.cells[i], s.cells[i], "cell {i} differs");
        }
    }

    #[test]
    fn chunked_writes_survive_split_escapes() {
        // Emulate crossterm splitting a single command across write() calls.
        let mut whole = Screen::blank(20, 1);
        draw_cell(&mut whole, 0, 0, 20, "split me", theme().amber, Some(theme().bg), true).unwrap();
        whole.flush_ground();

        // capture the exact bytes, then feed them one at a time
        let mut prev = Screen::blank(20, 1);
        let mut bytes = Vec::new();
        whole.flush_into(&mut prev, &mut bytes).unwrap();
        let mut byte_at_a_time = Screen::blank(20, 1);
        for b in &bytes {
            byte_at_a_time.write_all(&[*b]).unwrap();
        }
        byte_at_a_time.flush_ground();
        assert_eq!(grid_text(&byte_at_a_time), grid_text(&whole));
    }

    #[test]
    fn diff_emits_only_changed_cells() {
        // frame 1
        let mut prev = Screen::blank(30, 2);
        let mut frame1 = Screen::blank(30, 2);
        draw_cell(
            &mut frame1,
            0,
            0,
            30,
            "unchanged header",
            theme().paper,
            Some(theme().bg),
            false,
        )
        .unwrap();
        draw_cell(&mut frame1, 0, 1, 30, "status: idle", theme().dim, Some(theme().bg), false).unwrap();
        let mut sink = Vec::new();
        frame1.flush_into(&mut prev, &mut sink).unwrap();

        // frame 2: only the second line changes
        let mut frame2 = Screen::blank(30, 2);
        frame2.cells.clone_from(&prev.cells);
        frame2.begin_frame();
        draw_cell(
            &mut frame2,
            0,
            1,
            30,
            "status: busy",
            theme().amber,
            Some(theme().bg),
            false,
        )
        .unwrap();
        let mut sink2 = Vec::new();
        frame2.flush_into(&mut prev, &mut sink2).unwrap();
        let out = String::from_utf8_lossy(&sink2);

        // the header text must not be re-emitted; the changed word must be
        assert!(
            !out.contains("unchanged header"),
            "unchanged row was repainted: {out:?}"
        );
        assert!(
            out.contains("busy"),
            "changed row was not repainted: {out:?}"
        );
        // and the cursor should jump to row 2 (CUP row index 2)
        assert!(out.contains("\x1b[2;"), "expected a move to the second row");
    }

    #[test]
    fn diff_is_far_smaller_than_full_repaint() {
        // A representative full frame: 100x30 of colored cells.
        let (w, h) = (100u16, 30u16);
        let paint = |s: &mut Screen, spinner: char| {
            s.begin_frame();
            for y in 0..h {
                draw_cell(
                    s,
                    0,
                    y,
                    w,
                    &format!("row {y:02} of the board view here"),
                    theme().paper,
                    Some(theme().bg),
                    false,
                )
                .unwrap();
            }
            let mut x = 2;
            draw_span(
                s,
                &mut x,
                h - 1,
                &format!("working {spinner}"),
                theme().amber,
                Some(theme().bg),
                true,
            )
            .unwrap();
            s.flush_ground();
        };

        // full repaint = flushing frame 1 against a blank terminal
        let mut prev = Screen::blank(w, h);
        let mut frame1 = Screen::blank(w, h);
        paint(&mut frame1, '⣾');
        let mut full = Vec::new();
        frame1.flush_into(&mut prev, &mut full).unwrap();

        // next frame: only the spinner glyph changes (a typical tick)
        let mut frame2 = Screen::blank(w, h);
        frame2.cells.clone_from(&prev.cells);
        paint(&mut frame2, '⣽');
        let mut diff = Vec::new();
        frame2.flush_into(&mut prev, &mut diff).unwrap();

        println!(
            "full repaint = {} bytes, one-glyph diff = {} bytes ({}x smaller)",
            full.len(),
            diff.len(),
            full.len() / diff.len().max(1),
        );
        // the spinner change should cost a tiny fraction of a full repaint
        assert!(
            diff.len() * 20 < full.len(),
            "diff {} not much smaller than full {}",
            diff.len(),
            full.len()
        );
    }

    #[test]
    fn force_clear_erases_uncovered_cells() {
        // an overlay covers part of the screen...
        let mut prev = Screen::blank(20, 2);
        let mut with_overlay = Screen::blank(20, 2);
        draw_cell(
            &mut with_overlay,
            0,
            0,
            20,
            "base row zero here",
            theme().dim,
            Some(theme().bg),
            false,
        )
        .unwrap();
        draw_cell(
            &mut with_overlay,
            0,
            1,
            20,
            "OVERLAY POPUP",
            theme().paper,
            Some(theme().cell_bg),
            true,
        )
        .unwrap();
        let mut sink = Vec::new();
        with_overlay.flush_into(&mut prev, &mut sink).unwrap();

        // ...then it closes: a force_clear frame that only redraws row zero
        let mut cleared = Screen::blank(20, 2); // blank == what new_frame builds on force_clear
        cleared.begin_frame();
        draw_cell(
            &mut cleared,
            0,
            0,
            20,
            "base row zero here",
            theme().dim,
            Some(theme().bg),
            false,
        )
        .unwrap();
        let mut sink2 = Vec::new();
        cleared.flush_into(&mut prev, &mut sink2).unwrap();

        // row 1 in the terminal state must now be blank (overlay erased)
        for x in 0..prev.w {
            let c = &prev.cells[prev.idx(x, 1)];
            assert_eq!(c.ch, ' ', "overlay cell {x} not cleared");
        }
    }
}
