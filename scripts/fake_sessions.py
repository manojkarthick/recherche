# /// script
# requires-python = ">=3.11"
# ///
"""Made-up Claude Code and Codex sessions for trying out recherche.

Writes 17 sessions across four fake repos into a scratch directory, plus a
config and an env file that point recherche at them instead of your own
sessions. Dates are relative to now. The sessions cover summaries with
Markdown, titles taken from prompts, a subagent, Codex sessions, failing tool
calls, long URLs and hashes, non-ASCII text, PR links, tickets and a worktree
that no longer exists.

    uv run scripts/fake_sessions.py create /tmp/recherche-demo
    . /tmp/recherche-demo/env.sh
    cargo run -- index
    uv run scripts/fake_sessions.py summaries /tmp/recherche-demo
    cargo run -- index
    cargo run -- serve

`summaries` writes LLM-style summaries straight into the database, so no API
key is needed; run it after the first index. Models are shared with your
normal cache.
"""

import argparse
import json
import os
import random
import sqlite3
import uuid
from datetime import UTC, datetime, timedelta
from pathlib import Path

NOW = datetime.now(UTC).replace(microsecond=0)


def ts(t: datetime) -> str:
    return t.strftime("%Y-%m-%dT%H:%M:%S.000Z")


def ago(days: float, hours: float = 0) -> datetime:
    return NOW - timedelta(days=days, hours=hours)


class Writer:
    def __init__(self, out: Path):
        self.claude = out / "claude"
        self.codex = out / "codex"

    def claude_session(
        self, cwd, branch, start, turns, title=None, pr=None, agent=None, parent=None
    ):
        """turns: (prompt, reply, [(tool, input, output, is_error)])."""
        sid = parent or str(uuid.uuid4())
        model = "claude-opus-4-5"
        base = {"sessionId": sid, "cwd": cwd, "gitBranch": branch}
        if agent:
            base["agentId"] = agent
        t, lines, n = start, [], 0
        for prompt, reply, *rest in turns:
            lines.append(
                base
                | {
                    "type": "user",
                    "timestamp": ts(t),
                    "message": {"role": "user", "content": prompt},
                }
            )
            t += timedelta(minutes=random.randint(1, 6))
            for name, inp, output, err in rest[0] if rest else []:
                n += 1
                tid = f"toolu_{n:04d}"
                use = {"type": "tool_use", "id": tid, "name": name, "input": inp}
                result = {
                    "type": "tool_result",
                    "tool_use_id": tid,
                    "content": output,
                    "is_error": err,
                }
                lines.append(
                    base
                    | {
                        "type": "assistant",
                        "timestamp": ts(t),
                        "message": {"model": model, "content": [use]},
                    }
                )
                lines.append(
                    base
                    | {
                        "type": "user",
                        "timestamp": ts(t),
                        "message": {"role": "user", "content": [result]},
                    }
                )
                t += timedelta(seconds=40)
            text = {"type": "text", "text": reply}
            lines.append(
                base
                | {
                    "type": "assistant",
                    "timestamp": ts(t),
                    "message": {"model": model, "content": [text]},
                }
            )
            t += timedelta(minutes=random.randint(2, 15))
        if title and not agent:
            lines.append({"type": "ai-title", "aiTitle": title, "sessionId": sid})
        if pr:
            lines.append(
                {
                    "type": "pr-link",
                    "prRepository": pr[0],
                    "prNumber": pr[1],
                    "sessionId": sid,
                }
            )
        project = self.claude / "projects" / cwd.replace("/", "-")
        if agent:
            # Subagents live next to their parent and are named by a meta file.
            path = project / parent / "subagents" / f"agent-{agent}.jsonl"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.with_suffix(".meta.json").write_text(
                json.dumps({"description": title})
            )
        else:
            path = project / f"{sid}.jsonl"
            path.parent.mkdir(parents=True, exist_ok=True)
        write_jsonl(path, lines, t)
        return sid

    def codex_session(self, cwd, repo, branch, start, turns):
        """turns: (prompt, reply, [(command, output, exit_code)])."""
        sid = str(uuid.uuid4())
        t = start
        commit = f"{random.getrandbits(160):040x}"
        git = {
            "repository_url": f"git@github.com:{repo}.git",
            "branch": branch,
            "commit_hash": commit,
        }
        lines = [
            {
                "timestamp": ts(t),
                "type": "session_meta",
                "payload": {"id": sid, "cwd": cwd, "source": "cli", "git": git},
            },
            {
                "timestamp": ts(t),
                "type": "turn_context",
                "payload": {"cwd": cwd, "model": "gpt-5.2-codex"},
            },
        ]

        def item(item):
            return {
                "timestamp": ts(t),
                "type": "event_msg",
                "payload": {"type": "item_completed", "item": item},
            }

        for prompt, reply, *rest in turns:
            lines.append(
                item(
                    {
                        "type": "UserMessage",
                        "content": [{"type": "text", "text": prompt}],
                    }
                )
            )
            t += timedelta(minutes=2)
            for command, output, code in rest[0] if rest else []:
                lines.append(
                    item(
                        {
                            "type": "CommandExecution",
                            "command": ["bash", "-lc", command],
                            "exit_code": code,
                            "aggregated_output": output,
                        }
                    )
                )
                t += timedelta(seconds=30)
            lines.append(
                item(
                    {
                        "type": "AgentMessage",
                        "content": [{"type": "text", "text": reply}],
                    }
                )
            )
            t += timedelta(minutes=random.randint(3, 12))
        day = self.codex / "sessions" / start.strftime("%Y/%m/%d")
        day.mkdir(parents=True, exist_ok=True)
        stamp = start.strftime("%Y-%m-%dT%H-%M-%S")
        write_jsonl(day / f"rollout-{stamp}-{sid}.jsonl", lines, t)
        return sid


def write_jsonl(path: Path, lines: list, mtime: datetime):
    path.write_text("".join(json.dumps(line) + "\n" for line in lines))
    # Match the file time to the session's end, as a real session file would.
    os.utime(path, (mtime.timestamp(), mtime.timestamp()))


def push_output(repo: str, branch: str) -> str:
    return (
        f"remote: Create a pull request for '{branch}' on GitHub by visiting:\n"
        f"remote:      https://github.com/{repo}/pull/new/{branch}\n"
        f"To github.com:{repo}.git\n"
        f" * [new branch]      {branch} -> {branch}"
    )


MARKDOWN_REPLY = """Found it. The ordering bug comes from `merge_batches()` sorting by `created_at` **after** deduplication, so two rows with the same timestamp keep whichever batch arrived first.

## What changed

1. Sort by `(created_at, seq)` before deduplicating.
2. Keep the *latest* row per `order_id` instead of the first.
3. Added a regression test with two rows that share a timestamp.

| Case | Before | After |
| --- | --- | --- |
| Same timestamp, two batches | first batch wins | highest `seq` wins |
| Out-of-order arrival | sorted late | sorted before dedupe |
| Empty batch | panics on `unwrap()` | returns `[]` |

```rust
let mut rows: Vec<Row> = batches.into_iter().flatten().collect();
rows.sort_by_key(|r| (r.created_at, r.seq));
rows.dedup_by(|a, b| a.order_id == b.order_id); // keeps the later row because we reversed first
```

> Note: `dedup_by` keeps the *first* of each run, so the vector is reversed before and after.

Tests pass: `cargo test -p ingest` → 48 passed. Anything else on this branch?"""

# (cwd under home, repo, branch, title, first prompt, kind)
FILLER = [
    ("widgets", "acme/widgets", "feat/WID-1203-csv-export", "Add CSV export to the widgets report", "add a CSV export button to the weekly report, same columns as the table", "feature"),
    ("widgets", "acme/widgets", "fix/flaky-snapshot-test", "Flaky snapshot test in dashboard", "the dashboard snapshot test fails about one run in five on CI, can you find out why", "bugfix"),
    ("billing-api", "northwind/billing-api", "feat/BILL-77-proration", "Prorate mid-cycle plan changes", "BILL-77: customers who upgrade mid-cycle get charged the full month. prorate it", "feature"),
    ("billing-api", "northwind/billing-api", "chore/upgrade-postgres-16", "Upgrade Postgres to 16 in docker-compose", "bump postgres to 16 in the compose file and check the migrations still run", "chore"),
    ("billing-api", "northwind/billing-api", "fix/webhook-retry-storm", "Stop webhook retry storm after outage", "after the outage the webhook worker retried everything at once and hit the rate limit. add jitter", "bugfix"),
    ("notebook", "lumen/notebook", "feat/markdown-tables", "Markdown table editing in notes", "let people edit markdown tables inline, at least add/remove rows", "feature"),
    ("notebook", "lumen/notebook", "docs/sync-architecture", "Write up sync architecture", "write a doc explaining how sync works now, for the new hire", "docs"),
    ("dotfiles", "sam/dotfiles", "main", "Tmux clipboard over SSH", "copy in tmux doesn't reach my mac clipboard when I'm ssh'd in", "config"),
    ("dotfiles", "sam/dotfiles", "main", "Neovim LSP slow on large repo", "nvim's rust-analyzer takes forever on the billing repo", "investigation"),
    ("widgets", "acme/widgets", "refactor/split-report-module", "Split report module into smaller files", "report.rs is 2400 lines, split it up without changing behavior", "refactor"),
    ("billing-api", "northwind/billing-api", "fix/BILL-102-tax-rounding", "Tax rounding off by one cent", "BILL-102 tax totals are a cent off for some EU invoices", "bugfix"),
    ("notebook", "lumen/notebook", "fix/ios-keyboard-overlap", "Keyboard covers editor on iOS", "on iPhone the keyboard covers the last lines of the note editor", "bugfix"),
]  # fmt: skip

OUTCOMES = ["fixed", "shipped", "abandoned", "partial", "answered"]


def create(out: Path):
    random.seed(7)
    home = out / "home"
    code = home / "code"
    for repo in ["widgets", "billing-api", "notebook", "dotfiles"]:
        (code / repo).mkdir(parents=True, exist_ok=True)
    w = Writer(out)
    summaries = []

    # A worktree that no longer exists, so its results warn "directory is gone".
    worktree = f"{code}/widgets/.claude/worktrees/fix-order-dedupe"
    branch = "fix/WID-1182-order-dedupe-same-timestamp"
    main = w.claude_session(
        worktree,
        branch,
        ago(1, 3),
        [
            (
                "Orders with the same created_at come back in the wrong order after the nightly merge. WID-1182. Can you find out why?",
                "Let me look at the merge code first.",
                [
                    ("Grep", {"pattern": "fn merge_batches", "path": "src"}, "src/ingest/merge.rs:41:pub fn merge_batches(batches: Vec<Batch>) -> Vec<Row> {", False),
                    ("Read", {"file_path": f"{worktree}/src/ingest/merge.rs"}, "41 pub fn merge_batches(batches: Vec<Batch>) -> Vec<Row> {\n42     let mut rows = dedupe(batches);\n43     rows.sort_by_key(|r| r.created_at);\n44     rows\n45 }", False),
                    ("Bash", {"command": "cargo test -p ingest merge"}, "running 6 tests\ntest merge::tests::same_timestamp ... FAILED\n\nfailures:\n---- merge::tests::same_timestamp stdout ----\nassertion `left == right` failed\n  left: [Row { order_id: 7, seq: 1 }]\n right: [Row { order_id: 7, seq: 2 }]", True),
                ],
            ),
            (
                "ok fix it and add a test",
                MARKDOWN_REPLY,
                [
                    ("Edit", {"file_path": f"{worktree}/src/ingest/merge.rs"}, "The file has been updated.", False),
                    ("Bash", {"command": "cargo test -p ingest"}, "test result: ok. 48 passed; 0 failed; 0 ignored", False),
                ],
            ),
            (
                "push it and open a PR",
                "Pushed and opened acme/widgets#418.",
                [
                    ("Bash", {"command": f"git push -u origin {branch}"}, push_output("acme/widgets", branch), False),
                    ("Bash", {"command": "gh pr create --fill"}, "https://github.com/acme/widgets/pull/418", False),
                ],
            ),
        ],
        title="Fix order dedupe for rows sharing a timestamp",
        pr=("acme/widgets", 418),
    )  # fmt: skip
    summaries.append(
        {
            "id": main,
            "title": "Fix order dedupe for rows sharing a timestamp",
            "summary": "- Nightly merge sorted rows after deduplicating, so the first batch won when two rows shared `created_at`.\n- Sorted by `(created_at, seq)` before dedupe and kept the latest row per `order_id`.\n- Added a regression test; opened acme/widgets#418.",
            "kind": "bugfix",
            "outcome": "fixed",
        }
    )
    w.claude_session(
        worktree,
        branch,
        ago(1, 2),
        [
            (
                "Find every caller of merge_batches and report whether any rely on first-wins ordering.",
                "Three callers; none rely on first-wins. `nightly_merge` and `backfill` pass sorted input; `replay` passes one batch.",
                [("Grep", {"pattern": "merge_batches\\(", "path": "src"}, "src/jobs/nightly.rs:22\nsrc/jobs/backfill.rs:58\nsrc/jobs/replay.rs:13", False)],
            )
        ],
        title="Check callers of merge_batches",
        agent="a1b2c3d4",
        parent=main,
    )  # fmt: skip

    # No AI title and no summary: the title is the first prompt, cut short.
    w.claude_session(
        f"{code}/billing-api",
        "main",
        ago(3, 5),
        [
            (
                "I need to understand why the invoice PDF renderer sometimes produces a blank second page when the line items table is exactly as tall as the remaining space on the first page, and whether we should switch from the hand-rolled pagination to the layout engine's built-in page breaking which apparently handles widows and orphans now",
                "The blank page comes from the footer reserving 48pt after the table already filled the page. The layout engine's `break-inside: avoid` support would remove the hand-rolled code, but it changes how totals rows split.",
                [("Bash", {"command": "rg -n 'reserve_footer' src/pdf"}, "src/pdf/paginate.rs:112:    let remaining = page.height - cursor - reserve_footer(&doc);", False)],
            )
        ],
    )  # fmt: skip

    w.codex_session(
        f"{code}/notebook",
        "lumen/notebook",
        "feat/offline-sync",
        ago(0, 5),
        [
            (
                "why does offline sync duplicate notes when two devices reconnect at the same time",
                "Both devices replay their queue against the same base revision. The server accepts both because `If-Match` is checked against the note, not the queue entry. I added a revision check to `apply_queue`.",
                [
                    ("pnpm test -- sync", "FAIL src/sync/queue.test.ts\n  ● applyQueue › rejects stale revisions\n    expect(received).toBe(expected)\n    Expected: 409\n    Received: 200", 1),
                    ("pnpm test -- sync", "PASS src/sync/queue.test.ts (14 tests)", 0),
                ],
            ),
            (
                "commit that",
                "Committed on feat/offline-sync.",
                [("git commit -am 'fix(sync): reject stale queue entries'", "[feat/offline-sync 3e1a9c2] fix(sync): reject stale queue entries\n 2 files changed, 41 insertions(+), 6 deletions(-)", 0)],
            ),
        ],
    )  # fmt: skip

    # Content that is hard to lay out: a long URL, an unbroken hash, emoji,
    # CJK text and 60 lines of failing tool output.
    long_output = "\n".join(
        f" ERR_PNPM_TARBALL_INTEGRITY  line {i}: checksum mismatch for @acme/some-really-long-package-name-for-testing-wrapping@4.2.{i}"
        for i in range(60)
    )
    w.claude_session(
        f"{code}/widgets",
        "chore/deps",
        ago(9),
        [
            (
                "the build log says https://ci.example.com/acme/widgets/actions/runs/1234567890/job/9876543210?check_suite_focus=true&pr=422&very_long_query_param=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa failed 😩 — 依存関係の更新 broke something",
                "The failure is in the lockfile: `sha512-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKlMnOpQrStUvWxYz==` no longer matches.\n\n```\nERR_PNPM_TARBALL_INTEGRITY  Got unexpected checksum for \"https://registry.npmjs.org/@acme/some-really-long-package-name-for-testing-wrapping/-/some-really-long-package-name-for-testing-wrapping-4.2.1.tgz\". Wanted \"sha512-AAAA\". Got \"sha512-BBBB\".\n```\n\nRegenerating the lockfile fixes it.",
                [("Bash", {"command": "pnpm install --frozen-lockfile 2>&1 | tail -40"}, long_output, True)],
            )
        ],
        title="Lockfile checksum mismatch after dependency bump",
    )  # fmt: skip

    for i, (repo_dir, repo, branch, title, prompt, kind) in enumerate(FILLER):
        cwd = f"{code}/{repo_dir}"
        start = ago(2 + i * 9, i)
        reply = "I looked at the relevant code and made the change.\n\n- Updated the handler\n- Added tests\n\nRun `make test` to check."
        if i % 3 == 2:
            sid = w.codex_session(
                cwd,
                repo,
                branch,
                start,
                [
                    (
                        prompt,
                        f"Done. I traced it to `{branch.split('/')[-1]}` and changed two files. Tests pass.",
                        [("make test", "ok  all 112 tests passed", 0)],
                    )
                ],
            )
        else:
            sid = w.claude_session(
                cwd,
                branch,
                start,
                [
                    (
                        prompt,
                        reply,
                        [
                            (
                                "Bash",
                                {"command": f"git push -u origin {branch}"},
                                push_output(repo, branch),
                                False,
                            )
                        ],
                    ),
                    ("thanks, looks good", "Glad it works."),
                ],
                title=title if i % 2 == 0 else None,
                pr=(repo, 400 + i) if i % 4 == 0 else None,
            )
        # Leave every third session unsummarized.
        if i % 3 != 1:
            summaries.append(
                {
                    "id": sid,
                    "title": title,
                    "summary": f"- {title} was requested and implemented on its own branch.\n- Tests were added and pass locally.",
                    "kind": kind,
                    "outcome": OUTCOMES[i % len(OUTCOMES)],
                }
            )

    (out / "summaries.json").write_text(json.dumps(summaries, indent=1))
    config = out / "config" / "recherche" / "config.toml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text("[llm]\nenabled = false\n")
    (out / "env.sh").write_text(
        f"export XDG_CONFIG_HOME={out}/config\n"
        f"export RECHERCHE_DATA_DIR={out}/data\n"
        f"export CLAUDE_CONFIG_DIR={out}/claude\n"
        f"export CODEX_HOME={out}/codex\n"
    )
    print(f"Wrote 17 sessions to {out}. Next: . {out}/env.sh && cargo run -- index")


def add_summaries(out: Path):
    db = out / "data" / "recherche.db"
    if not db.exists():
        raise SystemExit(f"{db} does not exist; run `recherche index` first.")
    summaries = json.loads((out / "summaries.json").read_text())
    with sqlite3.connect(db) as c:
        for s in summaries:
            # dirty = 1 makes the next index pass reindex the summary text.
            c.execute(
                "UPDATE sessions SET llm_title = ?, summary = ?, kind = ?, outcome = ?, tags = '', summary_turns = turn_count, dirty = 1 WHERE id = ?",
                (s["title"], s["summary"], s["kind"], s["outcome"], s["id"]),
            )
    print(
        f"Added {len(summaries)} summaries. Run `recherche index` again to index them."
    )


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("command", choices=["create", "summaries"])
    p.add_argument("dir", type=Path, help="scratch directory for the fake sessions")
    a = p.parse_args()
    out = a.dir.resolve()
    if a.command == "create":
        if out.exists() and any(out.iterdir()):
            raise SystemExit(f"{out} is not empty; pick a new directory.")
        out.mkdir(parents=True, exist_ok=True)
        create(out)
    else:
        add_summaries(out)


if __name__ == "__main__":
    main()
