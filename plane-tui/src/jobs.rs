//! Agent cockpit job runtime (spec v4, phase 1).
//!
//! A job is a directory under `<data>/jobs/<id>/` — job.json, prompt.md,
//! run.sh, log.txt, result.md, exit — plus a tmux session that owns the
//! agent process. The TUI never holds the process: it polls these files on
//! its existing tick, so jobs survive TUI restarts and can be entered with
//! `tmux switch-client` / `attach` (deep dive).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const TAIL_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Waiting for the fable-5 brief generator; promoted to Queued when done.
    Briefing,
    Queued,
    Running,
    Review,
    Question,
    Failed,
    Orphaned,
    Landed,
    Discarded,
}

impl JobStatus {
    pub fn is_active(self) -> bool {
        !matches!(self, JobStatus::Landed | JobStatus::Discarded)
    }

    pub fn label(self) -> &'static str {
        match self {
            JobStatus::Briefing => "BRIEFING",
            JobStatus::Queued => "QUEUED",
            JobStatus::Running => "RUNNING",
            JobStatus::Review => "REVIEW",
            JobStatus::Question => "QUESTION",
            JobStatus::Failed => "FAILED",
            JobStatus::Orphaned => "ORPHANED",
            JobStatus::Landed => "LANDED",
            JobStatus::Discarded => "DISCARDED",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobMode {
    #[default]
    Headless,
    /// Full interactive claude session in the tmux pane — deep dive is a
    /// conversation. Human-paced: exempt from stall/timeout supervision.
    Interactive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub item_key: String,
    pub item_id: String,
    pub project_id: String,
    pub title: String,
    pub backend: String, // "claude" | "codex"
    pub model: String,
    pub effort: String,
    pub attempt: u32,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub base_ref: String,
    /// None = jobs live on the tmux server the TUI runs inside (resident
    /// deployment, switch-client deep dive); Some(name) = dedicated `-L` socket.
    pub tmux_socket: Option<String>,
    pub tmux_session: String,
    pub status: JobStatus,
    pub created_at: String,
    /// RFC3339 of the most recent spawn (set per attempt); None while queued.
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub mode: JobMode,
}

/// Runtime view of a job: the serialized Job plus tailing state.
pub struct JobHandle {
    pub dir: PathBuf,
    pub job: Job,
    pub log_offset: usize,
    pub tail: Vec<String>,
    /// Cached at the Running→Review transition so drawing never shells out.
    pub diff_stat: Option<String>,
    /// Last time the log grew (baselined at spawn/scan) — stall detection.
    pub last_activity: Option<std::time::Instant>,
    pub stalled: bool,
}

impl JobHandle {
    pub fn new(dir: PathBuf, job: Job) -> Self {
        Self {
            dir,
            job,
            log_offset: 0,
            tail: Vec::new(),
            diff_stat: None,
            last_activity: None,
            stalled: false,
        }
    }
}

pub fn jobs_root(data_dir: &Path) -> PathBuf {
    data_dir.join("jobs")
}

pub fn session_name(item_key: &str, attempt: u32) -> String {
    format!("pti-{}-a{attempt}", item_key.to_lowercase())
}

/// Resident deployment: when the TUI itself runs inside tmux, spawn jobs on
/// the same server so deep dive is a switch-client. Standalone: own socket.
pub fn default_socket() -> Option<String> {
    if std::env::var_os("TMUX").is_some() {
        None
    } else {
        Some("plane-tui".to_owned())
    }
}

fn tmux_command(socket: &Option<String>) -> Command {
    let mut command = Command::new("tmux");
    if let Some(socket) = socket {
        command.arg("-L").arg(socket);
    }
    command
}

fn run_quiet(mut command: Command) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {:?}", command.get_program()))?;
    if !output.status.success() {
        bail!(
            "{:?} failed: {}",
            command.get_program(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn run_capture(mut command: Command) -> Result<String> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {:?}", command.get_program()))?;
    if !output.status.success() {
        bail!(
            "{:?} failed: {}",
            command.get_program(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

// ------------------------------------------------------------ persistence

pub fn save(dir: &Path, job: &Job) -> Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join("job.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(job)?)?;
    fs::rename(&tmp, dir.join("job.json"))?;
    Ok(())
}

pub fn load(dir: &Path) -> Result<Job> {
    let raw = fs::read_to_string(dir.join("job.json"))?;
    serde_json::from_str(&raw).context("parsing job.json")
}

/// Rebuild all handles from disk — what a fresh TUI session does at startup.
pub fn scan(root: &Path) -> Vec<JobHandle> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut handles: Vec<JobHandle> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let job = load(&dir).ok()?;
            let mut handle = JobHandle::new(dir, job);
            pump(&mut handle); // replay log into the tail, settle status
            if handle.job.status == JobStatus::Briefing {
                // the brief generator died with the previous TUI process
                if handle.dir.join("prompt.md").exists() {
                    handle.job.status = JobStatus::Queued;
                } else {
                    handle.job.status = JobStatus::Failed;
                    handle
                        .tail
                        .push("brief was lost when the TUI exited — x discard, then d again".to_owned());
                }
                let _ = save(&handle.dir, &handle.job);
            }
            if matches!(handle.job.status, JobStatus::Review | JobStatus::Question) {
                handle.diff_stat = Some(diff_stat(&handle.job));
            }
            Some(handle)
        })
        .collect();
    handles.sort_by(|a, b| a.job.id.cmp(&b.job.id));
    handles
}

// -------------------------------------------------------------- worktrees

/// Create the job's worktree + branch off the repo's current HEAD.
/// Returns the base ref the diff is later computed against.
pub fn create_worktree(repo: &Path, worktree: &Path, branch: &str) -> Result<String> {
    if worktree.exists() {
        bail!("worktree {} already exists", worktree.display());
    }
    let mut rev_parse = Command::new("git");
    rev_parse.arg("-C").arg(repo).args(["rev-parse", "HEAD"]);
    let base_ref = run_capture(rev_parse).context("repo HEAD")?;
    if let Some(parent) = worktree.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut add = Command::new("git");
    add.arg("-C")
        .arg(repo)
        .args(["worktree", "add"])
        .arg(worktree)
        .args(["-b", branch]);
    run_quiet(add).context("git worktree add")?;
    Ok(base_ref)
}

/// Committed changes on the job branch plus a dirty-file count.
pub fn diff_stat(job: &Job) -> String {
    let mut diff = Command::new("git");
    diff.arg("-C")
        .arg(&job.worktree)
        .args(["diff", "--stat", &format!("{}..HEAD", job.base_ref)]);
    let committed = run_capture(diff).unwrap_or_else(|err| format!("(diff failed: {err})"));
    let mut status = Command::new("git");
    status
        .arg("-C")
        .arg(&job.worktree)
        .args(["status", "--short"]);
    let dirty = run_capture(status)
        .map(|out| out.lines().count())
        .unwrap_or(0);
    let mut text = if committed.is_empty() {
        "no committed changes".to_owned()
    } else {
        committed
    };
    if dirty > 0 {
        text.push_str(&format!("\n({dirty} uncommitted file(s) in the worktree)"));
    }
    text
}

/// Thread reviewer feedback into prompt.md for the next attempt: previous
/// result + note are appended, so the whole conversation lives in the job dir
/// and survives backend/model switches (spec v2 §4 — no native resume needed).
pub fn append_feedback(dir: &Path, finished_attempt: u32, note: &str) -> Result<()> {
    let result = read_result(dir);
    let result = if result.trim().is_empty() {
        "(no result captured)"
    } else {
        result.trim()
    };
    let mut prompt = fs::read_to_string(dir.join("prompt.md")).unwrap_or_default();
    prompt.push_str(&format!(
        "\n\n## attempt {finished_attempt} result\n{result}\n\n## reviewer feedback on attempt {finished_attempt}\n{note}\n\nAddress the feedback. Your previous commits are still in the worktree — build on them.\n"
    ));
    fs::write(dir.join("prompt.md"), prompt)?;
    Ok(())
}

/// Reset per-attempt files so the monitor doesn't instantly re-settle a
/// freshly respawned job on the previous attempt's exit code.
pub fn reset_attempt_files(dir: &Path) {
    let _ = fs::remove_file(dir.join("exit"));
    let _ = fs::remove_file(dir.join("result.md"));
}

/// The repo checkout's current branch — what land `m` merges into.
pub fn repo_head_branch(repo: &Path) -> Result<String> {
    let mut head = Command::new("git");
    head.arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "--short", "HEAD"]);
    run_capture(head).context("repo HEAD branch")
}

/// Land `m`: rebase the job branch onto the repo's branch (inside the job's
/// worktree — the main checkout is only touched by the final ff-merge), then
/// fast-forward the repo and clean up branch + worktree. A conflicting rebase
/// aborts cleanly and reports, so `f` can send the agent back to resolve it.
pub fn land_merge(job: &Job) -> Result<String> {
    let target = repo_head_branch(&job.repo)?;
    let mut rebase = Command::new("git");
    rebase
        .arg("-C")
        .arg(&job.worktree)
        .args(["rebase", &target]);
    if let Err(err) = run_quiet(rebase) {
        let mut abort = Command::new("git");
        abort
            .arg("-C")
            .arg(&job.worktree)
            .args(["rebase", "--abort"]);
        let _ = run_quiet(abort);
        bail!("rebase onto {target} conflicts — f sends the agent back to resolve ({err})");
    }
    let mut merge = Command::new("git");
    merge
        .arg("-C")
        .arg(&job.repo)
        .args(["merge", "--ff-only", &job.branch]);
    run_quiet(merge).with_context(|| format!("ff-merge {} into {target}", job.branch))?;
    kill_session(job);
    let mut remove = Command::new("git");
    remove
        .arg("-C")
        .arg(&job.repo)
        .args(["worktree", "remove", "--force"])
        .arg(&job.worktree);
    run_quiet(remove).context("git worktree remove")?;
    let mut delete = Command::new("git");
    delete
        .arg("-C")
        .arg(&job.repo)
        .args(["branch", "-d", &job.branch]);
    let _ = run_quiet(delete); // merged, so -d; a failure here is cosmetic
    Ok(target)
}

/// Land `b`/`P`: push the branch (worktree kept until the PR merges);
/// `P` additionally opens a PR via `gh` and returns its URL.
pub fn land_push(job: &Job, create_pr: bool) -> Result<String> {
    let mut push = Command::new("git");
    push.arg("-C")
        .arg(&job.worktree)
        .args(["push", "-u", "origin", &job.branch]);
    run_quiet(push).context("git push")?;
    if create_pr {
        let mut pr = Command::new("gh");
        pr.args(["pr", "create", "--head"])
            .arg(&job.branch)
            .arg("--title")
            .arg(format!("{}: {}", job.item_key, job.title))
            .arg("--body")
            .arg(format!(
                "Agent-dispatched change for {} via plane-tui.\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)",
                job.item_key
            ))
            .current_dir(&job.repo);
        return run_capture(pr).context("gh pr create");
    }
    Ok(format!("pushed origin/{}", job.branch))
}

/// Land-lite: keep the branch, drop the worktree. Merging/PR stays
/// a human act — agents submit, humans conclude.
pub fn accept(job: &Job) -> Result<()> {
    kill_session(job);
    let mut remove = Command::new("git");
    remove
        .arg("-C")
        .arg(&job.repo)
        .args(["worktree", "remove", "--force"])
        .arg(&job.worktree);
    run_quiet(remove).context("git worktree remove")
}

/// Discard: drop the worktree and delete the branch.
pub fn discard(job: &Job) -> Result<()> {
    accept(job)?;
    let mut delete = Command::new("git");
    delete
        .arg("-C")
        .arg(&job.repo)
        .args(["branch", "-D", &job.branch]);
    run_quiet(delete).context("git branch -D")
}

// ------------------------------------------------------------------ spawn

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn stream_json_enabled() -> bool {
    std::env::var("PLANE_TUI_STREAM_JSON")
        .map(|value| value != "0")
        .unwrap_or(true)
}

fn agent_command(job: &Job, dir: &Path, claude_permission_mode: &str) -> String {
    let prompt = shell_quote(&dir.join("prompt.md").display().to_string());
    let result = shell_quote(&dir.join("result.md").display().to_string());
    if job.backend.as_str() == "codex" {
        return format!(
            "{} exec --sandbox workspace-write --output-last-message {result} < {prompt}",
            shell_quote(&job.model_binary()),
        );
    }
    let bin = shell_quote(&job.model_binary());
    let model = shell_quote(&job.model);
    let effort = shell_quote(&job.effort);
    let perm = shell_quote(claude_permission_mode);
    if job.mode == JobMode::Interactive {
        // full claude TUI with the brief preloaded; the human drives it
        return format!("{bin} --model {model} --effort {effort} --permission-mode {perm} \"$(cat {prompt})\"");
    }
    if stream_json_enabled() {
        // structured tail: JSONL events on the pty; result extracted from the
        // log after exit (extract_stream_result), so no tee here
        format!(
            "{bin} -p \"$(cat {prompt})\" --model {model} --effort {effort} --permission-mode {perm} --output-format stream-json --verbose < /dev/null"
        )
    } else {
        format!(
            "{bin} -p \"$(cat {prompt})\" --model {model} --effort {effort} --permission-mode {perm} < /dev/null | tee {result}"
        )
    }
}

impl Job {
    fn model_binary(&self) -> String {
        match self.backend.as_str() {
            "codex" => std::env::var("PLANE_TUI_CODEX_BIN").unwrap_or_else(|_| "codex".into()),
            _ => std::env::var("PLANE_TUI_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
        }
    }
}

/// Write run.sh and start the agent inside its tmux session. The wrapper's
/// last act is writing `exit` — the ORPHANED detector depends on that order.
pub fn spawn(job: &Job, dir: &Path, claude_permission_mode: &str) -> Result<()> {
    let script = format!(
        "#!/bin/bash\nset -u\ncd {wt}\n{cmd}\nstatus=${{PIPESTATUS[0]:-$?}}\necho $status > {exit}\n",
        wt = shell_quote(&job.worktree.display().to_string()),
        cmd = agent_command(job, dir, claude_permission_mode),
        exit = shell_quote(&dir.join("exit").display().to_string()),
    );
    fs::write(dir.join("run.sh"), script)?;
    // interactive sessions skip pipe-pane: the claude TUI's redraws would
    // bloat log.txt with escape soup, and the pane scrollback
    // (remain-on-exit) is the real record for a human-driven session
    let log = (job.mode == JobMode::Headless).then(|| dir.join("log.txt"));
    spawn_raw(
        &job.tmux_socket,
        &job.tmux_session,
        &job.worktree,
        &format!(
            "bash {}",
            shell_quote(&dir.join("run.sh").display().to_string())
        ),
        log.as_deref(),
    )
}

/// tmux mechanics, factored out so tests can run a plain shell payload.
/// `remain-on-exit` keeps the pane for post-mortem deep dives; `pipe-pane`
/// mirrors the pty to log.txt (redirecting stdout would blank the pane).
pub fn spawn_raw(
    socket: &Option<String>,
    session: &str,
    cwd: &Path,
    command: &str,
    log: Option<&Path>,
) -> Result<()> {
    let mut tmux = tmux_command(socket);
    tmux.args(["new-session", "-d", "-s", session, "-c"])
        .arg(cwd)
        .arg(command)
        .arg(";")
        .args(["set-option", "-t", session, "-w", "remain-on-exit", "on"]);
    if let Some(log) = log {
        tmux.arg(";")
            .args(["pipe-pane", "-t", session, "-o"])
            .arg(format!(
                "cat >> {}",
                shell_quote(&log.display().to_string())
            ));
    }
    run_quiet(tmux).context("tmux new-session")
}

pub fn session_alive(job: &Job) -> bool {
    let mut tmux = tmux_command(&job.tmux_socket);
    tmux.args(["has-session", "-t", &format!("={}", job.tmux_session)]);
    tmux.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn kill_session(job: &Job) {
    let mut tmux = tmux_command(&job.tmux_socket);
    tmux.args(["kill-session", "-t", &format!("={}", job.tmux_session)]);
    let _ = tmux
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ------------------------------------------------------------- monitoring

pub fn read_exit(dir: &Path) -> Option<i32> {
    fs::read_to_string(dir.join("exit"))
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
}

pub fn read_result(dir: &Path) -> String {
    fs::read_to_string(dir.join("result.md")).unwrap_or_default()
}

/// How a raw log line should appear in the fleet tail.
pub enum StreamLine {
    /// Not a stream-json event — show the line as-is.
    Raw,
    /// A recognized event with nothing worth showing (tool results, etc.).
    Skip,
    /// A recognized event, rendered human-readable.
    Show(String),
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Render one `claude -p --output-format stream-json` event for the tail:
/// tool calls become "→ Edit services/upload/queue.py", assistant text keeps
/// its first line. Anything unrecognized falls back to Raw — codex output and
/// plain-text claude pass straight through.
pub fn format_stream_event(line: &str) -> StreamLine {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return StreamLine::Raw;
    }
    let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
        return StreamLine::Raw;
    };
    match event.get("type").and_then(Value::as_str) {
        Some("system") => {
            let model = event.get("model").and_then(Value::as_str).unwrap_or("?");
            StreamLine::Show(format!("· session started — {model}"))
        }
        Some("assistant") => {
            let Some(content) = event.pointer("/message/content").and_then(Value::as_array)
            else {
                return StreamLine::Skip;
            };
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let arg = block
                            .get("input")
                            .and_then(|input| {
                                ["file_path", "command", "pattern", "path", "query", "url"]
                                    .iter()
                                    .find_map(|key| input.get(*key).and_then(Value::as_str))
                            })
                            .unwrap_or("");
                        return StreamLine::Show(clip(&format!("→ {name} {arg}"), 90));
                    }
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        let Some(first) = text.lines().find(|line| !line.trim().is_empty())
                        else {
                            return StreamLine::Skip;
                        };
                        return StreamLine::Show(clip(first.trim(), 90));
                    }
                    _ => {}
                }
            }
            StreamLine::Skip
        }
        Some("result") => StreamLine::Show("── final result received ──".to_owned()),
        Some(_) => StreamLine::Skip,
        None => StreamLine::Raw,
    }
}

/// stream-json mode has no tee'd result.md — recover the final message from
/// the last `result` event in the log after the agent exits.
pub fn extract_stream_result(dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(dir.join("log.txt")).ok()?;
    for line in raw.lines().rev() {
        let clean = strip_ansi(line);
        let trimmed = clean.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("result") {
            return event
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }
    None
}

/// One monitoring tick: tail new log bytes, then settle Running jobs into
/// Review / Question / Failed / Orphaned. Returns true when anything changed.
pub fn pump(handle: &mut JobHandle) -> bool {
    let mut changed = false;
    let bytes = fs::read(handle.dir.join("log.txt")).unwrap_or_default();
    if bytes.len() > handle.log_offset {
        let new = String::from_utf8_lossy(&bytes[handle.log_offset..]);
        for line in new.lines() {
            let clean = strip_ansi(line);
            let clean = clean.trim_end();
            if clean.is_empty() {
                continue;
            }
            match format_stream_event(clean) {
                StreamLine::Raw => {
                    handle.tail.push(clean.to_owned());
                    changed = true;
                }
                StreamLine::Show(rendered) => {
                    handle.tail.push(rendered);
                    changed = true;
                }
                StreamLine::Skip => {}
            }
        }
        if handle.tail.len() > TAIL_LINES {
            let excess = handle.tail.len() - TAIL_LINES;
            handle.tail.drain(..excess);
        }
        handle.log_offset = bytes.len();
        handle.last_activity = Some(std::time::Instant::now());
        handle.stalled = false;
    }
    if handle.job.status != JobStatus::Running {
        return changed;
    }
    if handle.last_activity.is_none() {
        // baseline for stall detection after a spawn or a TUI restart
        handle.last_activity = Some(std::time::Instant::now());
    }
    // exit file first: the wrapper writes it and THEN the pane dies, so a
    // pid-first check would misread a just-finished job as orphaned
    let next = if let Some(code) = read_exit(&handle.dir) {
        if code == 0 {
            if read_result(&handle.dir).trim().is_empty() {
                // stream-json runs don't tee a result — recover it from the log
                if let Some(result) = extract_stream_result(&handle.dir) {
                    let _ = fs::write(handle.dir.join("result.md"), result);
                }
            }
            if read_result(&handle.dir)
                .trim_start()
                .starts_with("QUESTION:")
            {
                Some(JobStatus::Question)
            } else {
                Some(JobStatus::Review)
            }
        } else {
            Some(JobStatus::Failed)
        }
    } else if !session_alive(&handle.job) {
        Some(JobStatus::Orphaned)
    } else {
        None
    };
    if let Some(status) = next {
        handle.job.status = status;
        let _ = save(&handle.dir, &handle.job);
        changed = true;
    }
    changed
}

// -------------------------------------------------------------- deep dive

pub enum DeepDive {
    Switched,
    SpawnedTerminal(String),
    CopyCommand(String),
}

pub fn attach_command(job: &Job) -> String {
    match &job.tmux_socket {
        Some(socket) => format!("tmux -L {socket} attach -t {}", job.tmux_session),
        None => format!("tmux attach -t {}", job.tmux_session),
    }
}

/// Resident (TUI inside tmux, jobs on the same server): switch-client.
/// Standalone with a terminal command configured: spawn a window.
/// Otherwise: hand back the attach command for the caller to copy.
pub fn deep_dive(job: &Job, terminal_cmd: Option<&str>) -> Result<DeepDive> {
    if std::env::var_os("TMUX").is_some() && job.tmux_socket.is_none() {
        let mut tmux = tmux_command(&None);
        tmux.args(["switch-client", "-t", &job.tmux_session]);
        run_quiet(tmux).context("tmux switch-client")?;
        return Ok(DeepDive::Switched);
    }
    let attach = attach_command(job);
    if let Some(template) = terminal_cmd {
        let full = template.replace("{cmd}", &attach);
        let mut parts = full.split_whitespace();
        if let Some(program) = parts.next() {
            let spawned = Command::new(program)
                .args(parts)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if spawned.is_ok() {
                return Ok(DeepDive::SpawnedTerminal(full));
            }
        }
    }
    Ok(DeepDive::CopyCommand(attach))
}

// ------------------------------------------------------------------ tools

/// Strip ANSI escape sequences (CSI and OSC) that pipe-pane captures raw.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if ch == '\r' {
                continue;
            }
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for follow in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&follow) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut prev = ' ';
                for follow in chars.by_ref() {
                    if follow == '\u{7}' || (prev == '\u{1b}' && follow == '\\') {
                        break;
                    }
                    prev = follow;
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn strips_csi_and_osc_sequences() {
        assert_eq!(strip_ansi("\u{1b}[1;32mok\u{1b}[0m"), "ok");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}text"), "text");
        assert_eq!(strip_ansi("plain\r"), "plain");
        assert_eq!(strip_ansi("a\u{1b}[2Kb"), "ab");
    }

    #[test]
    fn session_names_are_stable() {
        assert_eq!(session_name("TM-201", 1), "pti-tm-201-a1");
        assert_eq!(session_name("TM-201", 3), "pti-tm-201-a3");
    }

    #[test]
    fn job_json_roundtrip() {
        let job = Job {
            id: "20260706-tm-201".into(),
            item_key: "TM-201".into(),
            item_id: "uuid".into(),
            project_id: "uuid".into(),
            title: "upload retry loop".into(),
            backend: "codex".into(),
            model: "gpt-5.5".into(),
            effort: "high".into(),
            attempt: 2,
            repo: PathBuf::from("/repo"),
            worktree: PathBuf::from("/wt"),
            branch: "tm-201-fix".into(),
            base_ref: "abc123".into(),
            tmux_socket: Some("plane-tui".into()),
            tmux_session: session_name("TM-201", 2),
            status: JobStatus::Review,
            created_at: "2026-07-06T00:00:00Z".into(),
            started_at: None,
            mode: JobMode::Headless,
        };
        let dir = std::env::temp_dir().join(format!("pti-job-test-{}", std::process::id()));
        save(&dir, &job).unwrap();
        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.tmux_session, "pti-tm-201-a2");
        assert_eq!(loaded.status, JobStatus::Review);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_events_render_for_the_tail() {
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"services/upload/queue.py"}}]}}"#;
        match format_stream_event(tool) {
            StreamLine::Show(line) => assert_eq!(line, "→ Edit services/upload/queue.py"),
            _ => panic!("tool_use should render"),
        }
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Root cause found.\nDetails follow."}]}}"#;
        match format_stream_event(text) {
            StreamLine::Show(line) => assert_eq!(line, "Root cause found."),
            _ => panic!("text should render"),
        }
        assert!(matches!(
            format_stream_event(r#"{"type":"result","subtype":"success","result":"done"}"#),
            StreamLine::Show(_)
        ));
        assert!(matches!(
            format_stream_event("plain codex output line"),
            StreamLine::Raw
        ));
        assert!(matches!(
            format_stream_event(r#"{"type":"user","message":{}}"#),
            StreamLine::Skip
        ));
    }

    #[test]
    fn stream_result_recovered_from_log() {
        let dir = std::env::temp_dir().join(format!("pti-stream-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("log.txt"),
            concat!(
                "{\"type\":\"system\",\"model\":\"gpt\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n",
                "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"Fixed the retry loop; 61 tests pass.\"}\n",
            ),
        )
        .unwrap();
        assert_eq!(
            extract_stream_result(&dir).as_deref(),
            Some("Fixed the retry loop; 61 tests pass.")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn feedback_threads_into_prompt() {
        let dir = std::env::temp_dir().join(format!("pti-fb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("prompt.md"), "# TM-9 · original brief\n").unwrap();
        fs::write(dir.join("result.md"), "QUESTION: renderer or workaround?").unwrap();
        fs::write(dir.join("exit"), "0").unwrap();
        append_feedback(&dir, 1, "firmware workaround please").unwrap();
        reset_attempt_files(&dir);
        let prompt = fs::read_to_string(dir.join("prompt.md")).unwrap();
        assert!(prompt.starts_with("# TM-9 · original brief"));
        assert!(prompt.contains("## attempt 1 result"));
        assert!(prompt.contains("QUESTION: renderer or workaround?"));
        assert!(prompt.contains("firmware workaround please"));
        assert!(!dir.join("exit").exists());
        assert!(!dir.join("result.md").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Full `m`-landing on a scratch repo: rebase, ff-merge, branch and
    /// worktree cleanup — the file the "agent" committed ends up on main.
    #[test]
    fn land_merge_fast_forwards_the_repo() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("pti-land-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let commit = |cwd: &Path, msg: &str| {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(cwd)
                .args(["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qam", msg]);
            run_quiet(command).unwrap();
        };
        let mut init = Command::new("git");
        init.arg("init").arg("-q").arg(&repo);
        run_quiet(init).unwrap();
        fs::write(repo.join("README.md"), "hello").unwrap();
        let mut add = Command::new("git");
        add.arg("-C").arg(&repo).args(["add", "."]);
        run_quiet(add).unwrap();
        commit(&repo, "init");

        let worktree = root.join("wt");
        let base_ref = create_worktree(&repo, &worktree, "tm-2-land").unwrap();
        fs::write(worktree.join("landed.txt"), "agent work").unwrap();
        let mut add = Command::new("git");
        add.arg("-C").arg(&worktree).args(["add", "landed.txt"]);
        run_quiet(add).unwrap();
        commit(&worktree, "agent change");
        // the repo moves on before landing — forces a real rebase
        fs::write(repo.join("README.md"), "hello, moved on").unwrap();
        commit(&repo, "mainline moved");

        let job = Job {
            id: "t".into(),
            item_key: "TM-2".into(),
            item_id: String::new(),
            project_id: String::new(),
            title: "land test".into(),
            backend: "claude".into(),
            model: String::new(),
            effort: String::new(),
            attempt: 1,
            repo: repo.clone(),
            worktree: worktree.clone(),
            branch: "tm-2-land".into(),
            base_ref,
            tmux_socket: Some("unused".into()),
            tmux_session: "pti-tm-2-a1".into(),
            status: JobStatus::Review,
            created_at: String::new(),
            started_at: None,
            mode: JobMode::Headless,
        };
        let target = land_merge(&job).unwrap();
        assert!(!target.is_empty());
        assert!(repo.join("landed.txt").exists(), "merge should land the file");
        assert!(!worktree.exists(), "worktree should be removed");
        let mut branches = Command::new("git");
        branches
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "tm-2-land"]);
        assert!(run_capture(branches).unwrap().is_empty(), "branch deleted");
        let _ = fs::remove_dir_all(&root);
    }

    /// Worktree lifecycle without tmux: branch off a scratch repo, commit as
    /// the agent would, read the diff stat, then discard cleanly.
    #[test]
    fn worktree_create_diff_and_discard() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("pti-wt-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let mut command = Command::new("git");
            command.arg("-C").arg(cwd).args(args);
            run_quiet(command).unwrap();
        };
        {
            let mut init = Command::new("git");
            init.arg("init").arg("-q").arg(&repo);
            run_quiet(init).unwrap();
        }
        fs::write(repo.join("README.md"), "hello").unwrap();
        git(&["add", "."], &repo);
        {
            let mut commit = Command::new("git");
            commit.arg("-C").arg(&repo).args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ]);
            run_quiet(commit).unwrap();
        }
        let worktree = root.join("wt");
        let base_ref = create_worktree(&repo, &worktree, "tm-1-test").unwrap();
        assert!(!base_ref.is_empty());
        fs::write(worktree.join("fix.txt"), "patch").unwrap();
        git(&["add", "fix.txt"], &worktree);
        {
            let mut commit = Command::new("git");
            commit.arg("-C").arg(&worktree).args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "fix",
            ]);
            run_quiet(commit).unwrap();
        }
        let job = Job {
            id: "t".into(),
            item_key: "TM-1".into(),
            item_id: String::new(),
            project_id: String::new(),
            title: String::new(),
            backend: "claude".into(),
            model: String::new(),
            effort: String::new(),
            attempt: 1,
            repo: repo.clone(),
            worktree: worktree.clone(),
            branch: "tm-1-test".into(),
            base_ref,
            tmux_socket: Some("unused".into()),
            tmux_session: "pti-tm-1-a1".into(),
            status: JobStatus::Review,
            created_at: String::new(),
            started_at: None,
            mode: JobMode::Headless,
        };
        assert!(diff_stat(&job).contains("fix.txt"));
        discard(&job).unwrap();
        assert!(!worktree.exists(), "worktree should be removed");
        let mut branches = Command::new("git");
        branches
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "tm-1-test"]);
        assert!(
            run_capture(branches).unwrap().is_empty(),
            "branch should be deleted"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// End-to-end tmux mechanics with a shell payload standing in for the
    /// agent: spawn detached, tail via pipe-pane log, read the exit file,
    /// survive "TUI restart" (fresh handle from disk). Skips without tmux.
    #[test]
    fn tmux_spawn_monitor_and_exit() {
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("tmux not installed — skipping integration test");
            return;
        }
        let socket = Some(format!("pti-test-{}", std::process::id()));
        let dir = std::env::temp_dir().join(format!("pti-tmux-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut job = Job {
            id: "test".into(),
            item_key: "TM-999".into(),
            item_id: String::new(),
            project_id: String::new(),
            title: "test".into(),
            backend: "claude".into(),
            model: String::new(),
            effort: String::new(),
            attempt: 1,
            repo: dir.clone(),
            worktree: dir.clone(),
            branch: String::new(),
            base_ref: String::new(),
            tmux_socket: socket.clone(),
            tmux_session: session_name("TM-999", 1),
            status: JobStatus::Running,
            created_at: String::new(),
            started_at: None,
            mode: JobMode::Headless,
        };
        save(&dir, &job).unwrap();
        spawn_raw(
            &socket,
            &job.tmux_session,
            &dir,
            &format!(
                "sh -c 'printf \"line-one\\nline-two\\n\"; sleep 0.3; echo 0 > {}'",
                shell_quote(&dir.join("exit").display().to_string())
            ),
            Some(&dir.join("log.txt")),
        )
        .unwrap();
        fs::write(dir.join("result.md"), "did the thing").unwrap();

        // fresh handle, as after a TUI restart
        let mut handle = JobHandle::new(dir.clone(), job.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.job.status == JobStatus::Running && Instant::now() < deadline {
            pump(&mut handle);
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            handle.job.status,
            JobStatus::Review,
            "tail: {:?}",
            handle.tail
        );
        assert!(
            handle.tail.iter().any(|line| line.contains("line-one")),
            "pipe-pane log missing output: {:?}",
            handle.tail
        );
        // remain-on-exit keeps the pane around for post-mortem dives
        job.status = handle.job.status;
        assert!(session_alive(&job), "pane should linger after exit");
        kill_session(&job);
        assert!(!session_alive(&job));
        let mut kill_server = tmux_command(&socket);
        kill_server.arg("kill-server");
        let _ = run_quiet(kill_server);
        let _ = fs::remove_dir_all(&dir);
    }
}
