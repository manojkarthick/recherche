# Using recherche

[Back to the README](../README.md) · [Configuration](configuration.md) · [Architecture](architecture.md)

Start with `recherche serve` and open <http://127.0.0.1:7777>. The server imports
existing sessions and keeps its indexes current while you work. The CLI uses
the same local data and can also run without the server.

## Search by what you remember

You can search with ordinary language or with something exact from the work:

| You remember… | Example query |
| --- | --- |
| What you were trying to do | `the conversation where we investigated slow startup` |
| A ticket | `PROJ-123` |
| A PR | `https://github.com/example/project/pull/42` or `PR #42` |
| A file or error | `src/config.rs` or `ENOSPC` |
| A branch | `fix-login` |
| A phrase | `"connection refused"` |

Known tickets, PRs, commits, file paths, and branch identifiers receive strong
priority. A PR URL identifies its repository as well as its number; a bare PR
number can match multiple repositories. Quoted phrases boost ordered keyword
matches, rather than imposing an exact-match-only filter.

For conversational queries, recherche removes common filler and searches the
remaining terms. Semantic search can find related wording; the reranker then
compares the query with the most promising sessions. A session can match through
individual turns or through its session card, which combines its title,
metadata, and any available summary.

## Narrow the results

Add filters directly to a query in either interface:

| Filter | Example | Meaning |
| --- | --- | --- |
| `repo:` | `repo:example` | Restrict to a repository known to the archive |
| `agent:` | `agent:claude` | `claude`, `codex` or `antigravity` |
| `branch:` | `branch:fix-login` | Restrict to sessions with that branch |
| `since:` | `since:30d` | Include sessions with activity at or after the cutoff |
| `until:` | `until:2026-03-31` | Include sessions that started by the end of that date |
| `kind:` | `kind:bugfix` | Restrict by generated work category |
| `outcome:` | `outcome:fixed` | Restrict by generated outcome |

For example:

```sh
recherche search "ordering repo:example agent:codex since:30d"
recherche search "repo:example since:2026-03-01 until:2026-03-31"
```

With only filters and no search text, sessions are listed newest activity first.
Dates use `YYYY-MM-DD`. Durations accept `d`, `w`, `m`, and `y`; months mean 30
days and years mean 365 days. Calendar dates use local midnight; durations
count backward from the current time.

Natural language time references such as “last week” or “in March” boost results
from that period. Use `since:` and `until:` when you want to exclude other times.
“In example” filters to that repository if recherche recognizes the name; a
bare repository name is a ranking hint.

Categories are `bugfix`, `feature`, `design`, `migration`, `review`, `research`,
`ops`, `refactor`, and `docs`. Outcomes are `fixed`, `shipped`, `investigated`,
`planned`, and `abandoned`. These fields come from optional LLM summaries;
sessions without them do not match a `kind:` or `outcome:` filter.

## Read and resume a session

Open a result to read its prompts, replies, and captured tool calls. Related
repos, branches, PRs, and tickets link to other sessions. Parent and child links
connect agent sessions when the source records that relationship. Missing
working directories are indicated in the UI.

| Key | Action |
| --- | --- |
| `/` | Focus and select the search text |
| `j` / `k`, or arrow keys | Move between results when not typing |
| Down arrow from the search field | Select the first result |
| `Enter` | Open the selected result |
| `c` | Copy the selected result's resume command, or the current session's command |
| `Escape` | Leave a text field; on a session page, navigate back when not typing |

A resume command changes to the recorded working directory when one is known,
then invokes `claude --resume`, `codex resume` or `agy --conversation`. Copying does not execute it.
recherche's archive supports searching and reading; it does not restore deleted
worktrees or install archived sessions back into any agent. Claude subagent
results use their parent session's resume command.

## CLI reference

The commands below assume `recherche` is on your `PATH`. Use
`recherche --help` or `recherche <command> --help` for the full flag list.
`--config /absolute/path/config.toml` selects a different configuration file
for any command.

| Command | Purpose |
| --- | --- |
| `recherche serve` | Run the browser UI and background importer; `--listen` overrides the address |
| `recherche search "<query>"` | Search from the terminal |
| `recherche list --limit 25 --since 7d` | List recent sessions without running embedding models |
| `recherche open "<query>"` | Open the configured UI in a browser; the server must already be running |
| `recherche index` | Import changed session files and update indexes once |
| `recherche index --rebuild` | Reparse all tracked archived files, including those whose originals are gone, and rebuild keyword documents |
| `recherche summarize` | Summarize eligible sessions, including the older backlog |
| `recherche eval -v` | Compare ranking configurations against labeled queries |
| `recherche install-service` | Write a launchd agent or systemd user unit and print activation instructions |
| `recherche doctor` | Show paths, session and index counts, malformed-line counts, and API availability |

`search` normally catches up the index before searching. Use `--no-index` to
skip that work. `--repo`, `--agent`, `--since`, and `--limit` provide common
filters as CLI flags. Use `--no-vectors` and `--no-rerank` to disable those
ranking stages for that invocation. These flags do not disable embeddings
during the catch-up import; use the [search configuration](configuration.md#local-search-models)
to disable models throughout indexing and search.

`search --json` and `list --json` print JSON arrays to stdout, with logs and
counts on stderr. Both include `id`, `agent`, `title`, `repo`, `branch`, `cwd`,
`started`, `ended`, `turn_count`, `summary`, `kind`, `outcome`, and `resume`;
`search --json` also includes `score` and `reasons`. Timestamps are Unix
seconds; missing repositories, directories, branches, and summary fields are
`null`. Empty results print `[]`. `list` defaults to 10 sessions, with no time
cutoff, and supports `--repo`, `--agent`, `--since`, and `--limit`.

## Optional summaries and query expansion

Configure an API key as described in [configuration](configuration.md#optional-llm-features)
to enable these features. They incur provider charges.

In the browser, sentence-like queries with weak local results can trigger
automatic expansion into related terms. CLI search opts into expansion with
`--rewrite`:

```sh
recherche search "where we investigated the history table problem" --rewrite
```

The expanded terms add retrieval candidates; the local reranker still scores
against your original query. If expansion fails, local results remain available.

Automatic summaries apply to top-level sessions started after the first
`serve` run, once idle for 20 minutes by default. The daemon checks every five
minutes and processes up to 20 candidates per check. Existing summaries are
refreshed only after substantial growth. Source and directory exclusions apply
to both automatic and manual summaries.

For older sessions, or to regenerate summaries:

```sh
recherche summarize --dry-run
recherche summarize
recherche summarize --redo 5 --dry-run
recherche summarize --redo 5
```

`--redo 5` selects the five most recently active eligible sessions, even if they
already have summaries or are still active. `--redo` without a number selects
all eligible sessions. `--dry-run` shows the count and, if the provider's model
list supplies prices, an estimated cost. It can contact the API for pricing but
does not submit summary requests.

You can leave `serve` running during manual summarization. Its next successful
refresh picks up the changes; reload the page to see them.

## Measure search quality

“This was it” on a result saves a positive label in the local database. These
labels become evaluation cases in the `feedback` class.

You can also create `eval.toml` in the [configuration directory](configuration.md#file-locations):

```toml
[[query]]
q = "the thread where we fixed the ordering issue"
class = "conversational"
expect = ["<session id>"]
```

Use real queries and replace the placeholder with a session ID from your archive.
Suggested classes are `conversational`, `exact`, `entity`, and `temporal`.
`recherche eval` compares keyword search, keyword plus vectors, and reranked
results. `--rewrite` adds API-assisted expansion; `--file` selects a different
case file.

Recall@5 and recall@10 report the fraction of queries with an expected session
in the first five or ten results. MRR, mean reciprocal rank, rewards placing
the first expected session closer to the top. `-v` prints each query's rank so
you can inspect regressions.
