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

- `1` `2` `3`: switch Product / iOS / Growth
- `j/k` and arrows: move
- `h/l`: board columns
- `v`: board/list view
- `D`: show/hide the Done board column
- `m`, `I`, `U`: mark, invert marks, clear marks
- `e`: edit the selected item title, description, or due date
- `a`: craft a "design and implement" agent prompt for the selected item (also `:agent`)
- `A`: same, then post the prompt back to the Plane item as a comment (also `:agent post`)
- `s`: set state on cursor or marks
- `p`: set priority on cursor or marks
- `t`: toggle labels on cursor or marks; press `n` in the label menu to create a label
- `T`: triage unprioritized backlog items
- `/`: search
- `:`: command mode, including `:new title`
- `x`: API drawer
- `?`: keys overlay
- `R`: refresh from Plane
- `o`: open canonical Plane browse URL
- `q`: quit

New items are created in the currently selected board state. In list view, they use the selected item's state.

## Agent prompts (`a` / `A`)

Press `a` on the selected work item to have Codex write a self-contained
"design and implement" prompt for a coding agent. The generated prompt combines
the item's fields (title, description, state, priority, labels, due, URL) with
embedded TranslateMom business/architecture context distilled from the internal
dossier (`src/business_context.md`, compiled into the binary).

The result opens in a scrollable overlay (`j/k` scroll, `y` copy, `esc` close),
is copied to the clipboard, and is saved under `~/.local/share/plane-tui/prompts/`.
Each run is also logged to the API drawer (`x`) as a `CODEX` entry.

`A` does everything `a` does and additionally posts the generated prompt back
to the work item as a Plane comment (wrapped in `<pre>` so formatting survives),
so the prompt lives with the task and can be grabbed from the Plane UI later.

Clipboard copies go through `pbcopy`/`wl-copy`/`xclip` when available and
always also emit an OSC 52 escape sequence, which asks the terminal itself
(kitty supports this by default) to set the clipboard — so copying works even
when plane-tui runs over SSH.

Configuration (flag / env var):

- `--codex-bin` / `PLANE_TUI_CODEX_BIN`: LLM CLI binary, default `codex`.
  Invoked as `codex exec --sandbox read-only --ephemeral` with the meta-prompt
  on stdin; the final message is read via `--output-last-message`.
- `--repo-dir` / `PLANE_TUI_REPO_DIR`: optional path to the TranslateMom
  monorepo checkout. When set, codex runs there (read-only) so it can ground
  the prompt in the real code before writing it.
- `--context-file` / `PLANE_TUI_CONTEXT_FILE`: optional plain-text/markdown
  file that replaces the embedded business context (e.g. a fresh `.md` export
  of the business dossier).
- `PLANE_TUI_PROMPT_DIR`: override the directory prompts are saved to.
