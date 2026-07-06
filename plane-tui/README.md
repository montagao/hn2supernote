# plane-tui

Rust terminal wrapper for Plane.so, implemented from `design/plane-tui.html`.

## Run

```sh
PLANE_API_KEY=... \
PLANE_API_URL=https://todo.translate.mom \
PLANE_WORKSPACE_SLUG=translatemom \
cargo run --manifest-path plane-tui/Cargo.toml
```

The local shell currently also accepts `PLANE_WORKPLACE_SLUG` for compatibility.

## Install

```sh
./plane-tui/scripts/install-pti.sh
pti
```

## Keys

- `tab` / `shift-tab`: next / previous project; `1` `2` `3`: direct project switch
- `j/k` and arrows: move
- `h/l`: board columns
- `enter`: item detail view — full description plus Plane comments (`wheel`/`j/k` scroll, `o` open, `a/A` agent prompt, `esc` close)
- `v`: board/list view
- `D`: show/hide the Done board column
- `m`, `I`, `U`: mark, invert marks, clear marks
- `e`: edit the selected item title, description (opens `$EDITOR`), or due date
- `d`: dispatch a coding agent on the selected item (agent cockpit — see below)
- `J`: fleet view for dispatched agents (`t` deep dive, `c` cancel, `r` retry,
  `l` land, `x` discard, `esc` close)
- `a`: craft a direct task brief for the selected item (also `:agent`)
- `A`: same, then post the prompt back to the Plane item as a comment (also `:agent post`)
- `esc`: cancel a running agent-prompt generation
- `s`: set state on cursor or marks; moving items into In Progress past the WIP limit
  asks for confirmation (`y` moves anyway, `esc`/`n` cancels)
- `p`: set priority on cursor or marks
- `t`: toggle labels on cursor or marks; press `n` in the label menu to create a label
- `T`: triage unprioritized backlog items
- `/`: search
- `:`: command mode, including `:new title`, `:project`, and `:backend`
- `x`: API drawer
- `?`: keys overlay
- `R`: refresh from Plane
- `o`: open canonical Plane browse URL (the inspector URL is also click-to-open via OSC 8)
- `q`: quit

New items are created in the currently selected board state. In list view, they use the
selected item's state. `:new` accepts inline tokens: `!u/!h/!m/!l` set priority and
`#labelname` attaches labels (case-insensitive, unique prefixes work), e.g.
`:new fix upload bug !h #fullstack`.

`:project` opens a two-step wizard for creating a Plane project: enter the display
name, then confirm or edit the generated project key before it is posted. Created
project keys are remembered in `~/.local/share/plane-tui/projects.tsv` so they are
merged into the `--projects` filter on later runs.

Cards show `Nd over` (red) or `due today`/`due tom` (amber) in place of the age when a due
date is close. The header shows `⟳Nm` sync age for the active project; the board
auto-refreshes when idle and stale (`--auto-refresh <minutes>` / `PLANE_TUI_AUTO_REFRESH`,
default 5, `0` disables).

The In Progress board column enforces a WIP limit (`--wip-limit` / `PLANE_TUI_WIP_LIMIT`,
default 2, `0` disables): its header shows `N/limit` and turns red when over, and both
`s` and the `T` triage flow guard against moving more items in past the limit.

## Agent prompts (`a` / `A`)

Press `a` on the selected work item to have an LLM CLI (Claude Code by default,
Codex optionally) write a direct task brief for a coding agent. The generated
brief combines
the item's fields (title, description, state, priority, labels, due, URL) with
embedded TranslateMom business/architecture context distilled from the internal
dossier (`src/business_context.md`, compiled into the binary).

Generation runs in the background — keep triaging while the spinner shows in the
status line; `esc` cancels. When it finishes, the prompt overlay pops up on its own.

The result opens in a scrollable overlay (`wheel`/`j/k` scroll, `y` copy, `esc` close),
is copied to the clipboard, and is saved under `~/.local/share/plane-tui/prompts/`.
Each run is also logged to the API drawer (`x`) as an `AGENT` entry.

`A` does everything `a` does and additionally posts the generated prompt back
to the work item as a Plane comment (wrapped in `<pre>` so formatting survives),
so the prompt lives with the task and can be grabbed from the Plane UI later.

Clipboard copies go through `pbcopy`/`wl-copy`/`xclip` when available and
always also emit an OSC 52 escape sequence, which asks the terminal itself
(kitty supports this by default) to set the clipboard — so copying works even
when plane-tui runs over SSH.

Configuration (flag / env var):

- `--agent-backend` / `PLANE_TUI_AGENT_BACKEND`: `claude` (default) or `codex`.
  Also configurable in-app with the `:backend` chooser, or directly with
  `:backend codex` / `:backend claude [model] [effort]`; in-app changes
  persist across restarts in
  `~/.local/share/plane-tui/agent-backend.tsv`. Flag/env, when given, wins over
  the persisted choice.
- `--claude-bin` / `PLANE_TUI_CLAUDE_BIN`: Claude Code binary, default `claude`.
  Invoked as `claude --print --model <model> --effort <effort>` with edits
  disabled via `--disallowedTools`; the brief is read from stdout.
- `--claude-model` / `PLANE_TUI_CLAUDE_MODEL`: default `claude-fable-5`.
- `--claude-effort` / `PLANE_TUI_CLAUDE_EFFORT`: default `high`
  (low/medium/high/xhigh/max).
- `--codex-bin` / `PLANE_TUI_CODEX_BIN`: Codex CLI binary, default `codex`.
  Invoked as `codex exec --sandbox read-only --ephemeral` with the meta-prompt
  on stdin; the final message is read via `--output-last-message`.
- `--repo-dir` / `PLANE_TUI_REPO_DIR`: optional path to the TranslateMom
  monorepo checkout. When set, the backend runs there (read-only) so it can
  ground the prompt in the real code before writing it.
- `--context-file` / `PLANE_TUI_CONTEXT_FILE`: optional plain-text/markdown
  file that replaces the embedded business context (e.g. a fresh `.md` export
  of the business dossier).
- `PLANE_TUI_PROMPT_DIR`: override the directory prompts are saved to.

## Agent cockpit (`d` / `J`)

`d` on a work item dispatches a coding agent to actually do the work (distinct
from `a`, which only writes a brief). The dispatch menu: `enter` takes the
default executor (codex, or the item's label default via
`PLANE_TUI_LABEL_EXECUTORS="frontend=claude,infra=codex"`), `1` codex,
`2` claude, `e` toggles the **stance** — `impl` (default: straightforward
implementation/bug fix) or `explore` (design-first: the agent must not touch
the real code paths; it delivers a committed design doc with its assumptions,
unknown unknowns, architecture-changing questions, and 2–3 prototyped options
with a recommendation, and its summary posts to the item). Review an
exploration, then `f` "go with option B — implement it" requeues into the
same worktree with the design threaded into the prompt. `s` opens a
**skills picker** — a checkbox list over installed skills (the target
repo's `.claude/skills` / `.codex/skills` first, then `~/.claude/skills`
and `~/.codex/skills`, deduped by name); picked skills are hinted in the
prompt by name + description ("use them where they genuinely help"), and
referenced in the fable-5 brief when `b` is on. `i` toggles
**interactive** mode for whichever executor you pick
(the pane runs the agent's own TUI — claude or codex — with the brief
preloaded; dispatching deep-dives you straight into it; human-paced, so
exempt from stall/timeout supervision), and `b` toggles
**two-stage briefing**: instead of the fast envelope prompt, the item first
goes to the `a`-flow brief generator (fable-5, repo-grounded) and the job
waits in BRIEFING until that brief becomes its prompt. Then type an optional
note and the cockpit:

1. creates a git worktree `$PLANE_TUI_WORKTREE_ROOT/<repo>-<key>` (default
   `~/projects/worktrees`) on branch `<key>-<slug>`, off `--repo-dir`'s HEAD;
2. writes the prompt to `~/.local/share/plane-tui/jobs/<id>/prompt.md` —
   item fields + description + a minimal cockpit contract (commit but never
   push; stop and ask with a leading `QUESTION:` line when blocked; end with
   a reviewer summary). Approach and conventions are left to the agent and
   the repo's own `CLAUDE.md`/`AGENTS.md`, which agent CLIs auto-load from
   the worktree; the embedded business-context dossier is injected only for
   repos without such docs (or when `--context-file` is set);
3. spawns the agent in a tmux session `pti-<key>-a<n>` — on the tmux server
   you're already inside when plane-tui runs in tmux (resident deployment),
   else on a dedicated `tmux -L plane-tui` socket. `remain-on-exit` keeps the
   pane for post-mortems; `pipe-pane` mirrors output to `log.txt`;
4. moves the item to In Progress and shows a live ⚑ badge on its card.

Jobs are files plus tmux sessions: quitting plane-tui does not touch them, and
the next launch re-attaches from `jobs/`. Up to `PLANE_TUI_AGENT_WIP` agents
(default 3) run at once; further dispatches queue and start automatically as
slots free. A running job with no output for `PLANE_TUI_STALL_MIN` minutes
(default 8) is flagged `⚠ STALLED?`; after `PLANE_TUI_JOB_TIMEOUT_MIN` minutes
(default 45, `0` disables) it is cancelled and marked failed.

On success the agent's final message is posted back to the Plane item as a
comment (retried in the background) and the job lands in REVIEW; a result
starting with `QUESTION:` shows as `?` instead. In the fleet (`J`):

- `enter`: full diff in git's pager (TUI suspends; quit the pager to return)
- `t`: deep dive into the live pane (`switch-client` when resident, else
  `$PLANE_TUI_TERMINAL_CMD`, default `kitty -e {cmd}`, else the attach
  command is copied to the clipboard)
- `f`: feedback → requeue. Pick the executor for the next attempt
  (`1`/`enter` keep current, `2` codex, `3` claude — one keystroke escalates
  a cheap attempt to the smart model), then type the note. The previous
  result plus your note are appended to `prompt.md`, and the same worktree
  keeps its commits. This is also how you answer a `?` QUESTION.
- `l`: land menu — `m` rebases the branch onto the repo's current branch,
  fast-forwards it, and removes branch + worktree (a conflicting rebase
  aborts cleanly and `f` can send the agent back to resolve it); `P` pushes
  and opens a PR via `gh`; `b` pushes the branch only. Every path marks the
  item Done and posts a landing comment.
- `c`: cancel a running job / remove a queued one · `r`: retry a failure ·
  `x`: discard (worktree removed, branch deleted)

Headless claude runs use `--output-format stream-json`, so the fleet tail
shows structured verbs (`→ Edit services/upload/queue.py`) instead of raw
text, and the final message is recovered from the event log
(`PLANE_TUI_STREAM_JSON=0` reverts to plain text if your claude CLI
misbehaves with it).

Other env vars: `PLANE_TUI_WORKTREE_ROOT` (default `~/projects/worktrees`),
`PLANE_TUI_CLAUDE_PERM` (executor permission mode, default `acceptEdits`;
set `bypassPermissions` if the agent should run tests unattended in its
disposable worktree), and `PLANE_TUI_LABEL_EXECUTORS` (per-label executor
defaults).
