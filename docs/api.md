# HTTP API

[Back to the README](../README.md) · [Usage](usage.md) · [Architecture](architecture.md)

`recherche serve` exposes a read-only JSON API under `/api`, by default at
`http://127.0.0.1:7777/api`. It shares the server's engine and authentication:
loopback requests need no token, and a non-loopback server uses the existing
query-token or cookie described in [Configuration](configuration.md), whose
failures are a plain-text `401`.

The API is unstable: it has no version prefix, and endpoints and fields can
change between releases without notice. Use it at your own risk, and expect
to update clients after upgrading. The implementation is in
[`src/api.rs`](../src/api.rs).

| Endpoint | Purpose |
| --- | --- |
| `GET /api/search` | Search, or browse recent sessions |
| `GET /api/sessions/{id}` | Session metadata, entities and resume arguments |
| `GET /api/sessions/{id}/turns` | Paginated transcript |
| `GET /api/entity/sessions` | Sessions associated with an entity |

## Conventions

- Responses are JSON with `Cache-Control: no-store`. Fields are `snake_case`;
  absent values are `null`, empty lists `[]`.
- Timestamps are Unix seconds. Session timestamps are `0` when unknown.
- Text is stored text or Markdown source, not rendered HTML.
- Session IDs and entity keys are opaque. URL-encode them, and send keys back
  exactly as Recherche returned them.

### Pagination

Collection endpoints accept `limit` (default `20`, `1..100`) and `offset`
(default `0`) and return:

```json
{
  "items": [],
  "total": 0,
  "page": {"limit": 20, "offset": 0, "next_offset": null}
}
```

`next_offset` is `null` when there is no further page. An offset past the end
returns an empty page. Pages are not snapshots; indexing between requests can
shift results.

### Errors

```json
{"error": {"code": "invalid_parameter", "message": "limit must be between 1 and 100"}}
```

| Status | Code | Meaning |
| --- | --- | --- |
| `400` | `invalid_parameter` | Missing or invalid parameter |
| `404` | `not_found` | Session is not in the searchable catalog |
| `500` | `internal_error` | Request could not be completed |

Branch on `code`, not the message.

## Session object

Returned by session details and entity lookups.

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | Native session ID |
| `agent` | string | Source agent, `claude`, `codex` or `antigravity` |
| `parent_id` | string or null | Parent session ID |
| `children` | string array | Direct child IDs, in lexical order |
| `host` | string | Recorded hostname; empty if unknown |
| `cwd` | string or null | Session working directory |
| `title` | string | Effective title (generated title when present) |
| `original_title` | string or null | Source title, after `ui.hide_system_instructions` |
| `generated_title` | string or null | LLM-generated title |
| `title_is_prompt` | boolean | Whether the source title came from prompt text |
| `recap` | string or null | Agent's latest recap |
| `summary` | string or null | Recherche-generated summary |
| `kind`, `outcome` | string or null | Work and outcome classification |
| `tags` | string array | Tags |
| `model` | string or null | Agent model |
| `started`, `ended` | integer | Start and last-activity time |
| `turn_count` | integer | Normalized turn count |
| `bytes` | integer | Session size |
| `bad_lines` | integer | Malformed JSON lines skipped on import |
| `summary_turns` | integer or null | Turns covered by the last summary |
| `repo_key` | string or null | Primary repository key |
| `repo` | string or null | Repository display name |
| `repo_origin` | string or null | How the primary repository was assigned |
| `entities` | object array | Entity associations, below |
| `resume_argv` | string array | Command that resumes the session |

`ended` is last recorded activity, not proof the agent has exited.

Each entity association is:

```json
{"kind": "pr", "key": "example/project#42", "display": "example/project#42", "origin": "parsed", "strength": 4}
```

Kinds are `repo`, `pr`, `ticket`, `branch`, `commit`, `sha`, `file` and
`worktree`. `origin` is `recorded`, `parsed` or `inferred`. For PRs, `strength`
is `4` opened, `3` linked, `2` pushed for, `1` mentioned.

`resume_argv` is the same command the browser and CLI show, without the `cd`:
`["claude", "--resume", "<id>"]`, where Claude subagents target their parent,
`["codex", "resume", "<id>"]`, or `["agy", "--conversation", "<id>"]`. Run it in `cwd` on `host`; the API does not
check that either is available and launches nothing.

## Search

`GET /api/search`

| Parameter | Meaning |
| --- | --- |
| `q` | Query, using the [search syntax](usage.md#narrow-the-results) |
| `repo`, `agent`, `branch`, `kind`, `outcome`, `since`, `until` | Browser filters |
| `limit`, `offset` | Pagination |

An empty `q` browses recent sessions. Search uses the configured vector and
reranking settings, without LLM query rewriting.

Besides the collection fields, the response has `stage`, `took_ms`, `notes`
(including model failures) and `confidence` (top rerank probability or `null`).
Each item is:

```json
{
  "session": {
    "id": "session-example",
    "agent": "claude",
    "title": "Investigate request ordering",
    "repo": "example/project",
    "repo_key": "github.com/example/project",
    "started": 1780315200,
    "ended": 1780318800,
    "parent_id": null
  },
  "score": 0.082,
  "reasons": ["exact ticket PROJ-123"],
  "rerank": 0.91,
  "matching_turn_indexes": [3]
}
```

Fetch `/api/sessions/{id}` for full metadata. `matching_turn_indexes` holds up
to two transcript indexes, best first; read one with
`/api/sessions/{id}/turns?offset={index}&limit=1`. Scores are not comparable
across queries, and `reasons` are display strings.

Only the top 100 hits are retrievable, so offsets of 100 or more return an empty
page. `total` can be larger: for ranked search it counts retrieved candidates,
for browsing all matching sessions.

## Session details

`GET /api/sessions/{id}` returns the session object, or `404`.

## Transcript

`GET /api/sessions/{id}/turns` returns turns in index order; `total` is the
session's turn count. A missing session returns `404`.

| Field | Type | Meaning |
| --- | --- | --- |
| `index` | integer | Zero-based turn index |
| `timestamp` | integer or null | Turn time |
| `branch`, `cwd` | string or null | Branch and directory for this turn |
| `user`, `assistant` | string | Prompt and reply text; `user` follows `ui.hide_system_instructions` |
| `tool_text` | string | Tool names, inputs and outputs, concatenated |
| `files`, `commands`, `errors` | string | Newline-joined lists |
| `tools` | object array | `{name, input, output, error}` per tool call |

Tool output keeps the clipping applied on import. Indexes can change if a source
is reparsed, so don't store them as permanent references.

## Sessions for an entity

`GET /api/entity/sessions?kind={kind}&key={key}` takes a nonempty `kind` and
`key` plus pagination, and returns session objects, most recently active first.
For `repo`, it matches sessions whose primary `repo_key` is the key; other kinds
match the key case-insensitively. An unknown entity returns an empty page.

## Tests

`cargo test --locked api::tests` runs the route tests against a synthetic
archive, without model inference or network access.
