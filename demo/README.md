# Demo sessions

`recherche demo` serves the made-up sessions in `sessions/`, so you can try UI
changes and take screenshots without showing your own work:

```sh
cargo run -- demo                          # /tmp/recherche-demo, http://127.0.0.1:7778
cargo run -- demo /tmp/other --listen 127.0.0.1:7779
```

Each run replaces the directory. It writes the sessions as Claude Code and Codex
would, indexes them, stores their summaries and starts the server. Your own
sessions and data are not touched. Summaries are on, so the UI explains missing
ones, but the API is a closed local port, so nothing is sent anywhere. The first
run of `serve` is dated 30 days back: older sessions without a summary show the
backfill note, newer ones show queued or in progress.

Debug builds read `sessions/` from disk, so a changed fixture shows up on the
next run without rebuilding. `cargo test demo` checks that every fixture loads
and indexes.

## Adding a session

Add a TOML file to `sessions/`. The file name is the session's identity: its ID
is derived from the name, and a subagent names its parent by it. Dates are
relative to now. `{root}` anywhere in the file becomes the demo directory, for
paths in tool inputs.

```toml
agent = "claude"                 # or "codex"
cwd = "code/widgets"             # relative to the demo directory
cwd_gone = false                 # true: leave it uncreated, like a deleted worktree
branch = "fix/WID-1182-ordering"
ago = "2d4h"                     # start time in d, h and m; activity in the last 20m is "in progress"
model = "claude-opus-4-5"        # optional

# Claude only
title = "Fix ordering"           # Claude Code's generated title; else the first prompt
pr = "acme/widgets#418"          # a PR the session opened
parent = "order-dedupe"          # makes this a subagent of sessions/order-dedupe.toml

# Codex only
repo = "acme/widgets"            # the remote Codex records

[[turns]]
prompt = "orders come back in the wrong order"
reply = '''
Markdown is fine here.'''

[[turns.tools]]
command = "cargo test"           # Bash for Claude, a command execution for Codex
output = "..."
error = true                     # a failed call, or a nonzero exit for Codex

[[turns.tools]]                  # Claude only: any tool by name
name = "Read"
input = { file_path = "{root}/code/widgets/src/merge.rs" }
output = "..."

[summary]                        # optional: stored as if the LLM wrote it
title = "Fix order dedupe for rows sharing a timestamp"
summary = '''
- One bullet per line.'''
kind = "bugfix"                  # one of llm::KINDS
outcome = "fixed"                # one of llm::OUTCOMES
tags = ["ordering"]
```

Recherche finds a Claude session's repo the way it does for real sessions: from
pushes and PR links in the session, or from the directory name.
