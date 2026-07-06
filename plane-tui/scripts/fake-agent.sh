#!/bin/sh
# Stand-in for the codex CLI so the cockpit pipeline can be smoke-tested
# without an LLM: accepts the same shape of invocation as
#   codex exec --sandbox workspace-write --output-last-message <path> < prompt.md
# reads the prompt, narrates for a bit, makes one commit in the worktree
# (cwd), writes a result, and exits 0.
#
# Use:  PLANE_TUI_CODEX_BIN=scripts/fake-agent.sh  (absolute path is safer)

out=""
while [ $# -gt 0 ]; do
    if [ "$1" = "--output-last-message" ] && [ $# -gt 1 ]; then
        out="$2"
        shift
    fi
    shift
done

prompt_bytes=$(wc -c < /dev/stdin | tr -d ' ')
echo "fake-agent: received brief (${prompt_bytes} bytes)"
echo "fake-agent: pretending to read the repo in $(pwd)"
sleep 2
echo "fake-agent: editing SMOKE_TEST.md"
echo "cockpit smoke test — safe to delete" > SMOKE_TEST.md
git add SMOKE_TEST.md
git -c user.email=fake@agent -c user.name=fake-agent \
    commit -qm "test: cockpit smoke commit" || echo "fake-agent: commit failed"
sleep 2
echo "fake-agent: running fake tests ..... ok"
sleep 2

if [ -n "$out" ]; then
    cat > "$out" <<'RESULT'
Smoke test result: created SMOKE_TEST.md with one line and committed it.
No real work was done — this validates dispatch → tmux → monitor →
comment → review → land end to end. Safe to land or discard.
RESULT
fi
echo "fake-agent: done"
exit 0
