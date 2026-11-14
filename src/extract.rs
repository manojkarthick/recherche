//! Entity extraction from session text: repos, branches, worktrees, commits,
//! PRs, tickets and files. Everything comes from the session itself.

use crate::config::Config;
use crate::model::{Fact, ParsedSession};
use regex::Regex;
use std::sync::LazyLock;

/// Normalizes a git remote to `host/owner/repo`, lowercased, without `.git`.
pub fn normalize_remote(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let rest = if let Some(r) = u.split_once("://").map(|x| x.1) {
        // Drop user info such as `git@` or `x-access-token:...@`.
        let r = r.rsplit_once('@').map(|x| x.1).unwrap_or(r);
        r.to_string()
    } else if let Some((user_host, path)) = u.split_once(':') {
        // scp-like: git@github.com:owner/repo
        let host = user_host.rsplit_once('@').map(|x| x.1).unwrap_or(user_host);
        if host.contains('/') {
            return None;
        }
        format!("{host}/{path}")
    } else {
        return None;
    };
    let rest = rest
        .split_once(':')
        .map(|(h, p)| {
            // Drop a port such as `host:22/owner/repo`.
            let p = p.split_once('/').map(|x| x.1).unwrap_or(p);
            format!("{h}/{p}")
        })
        .unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 3 || !parts[0].contains('.') {
        return None;
    }
    Some(parts.join("/").to_lowercase())
}

/// `github.com/owner/repo` → `owner/repo`; other hosts keep the host.
pub fn repo_display(key: &str) -> String {
    match key.strip_prefix("github.com/") {
        Some(r) => r.to_string(),
        None => key.strip_prefix("path:").unwrap_or(key).to_string(),
    }
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

re!(
    PR_URL,
    r"https?://(github\.com|gitlab\.[a-z.]+)/([\w.-]+)/([\w.-]+)/(?:pull|-/merge_requests)/(\d+)"
);
re!(
    PR_NEW,
    r"https?://github\.com/([\w.-]+)/([\w.-]+)/pull/new/([\w./-]+)"
);
re!(PUSH_TO, r"(?m)^To ((?:https?://|ssh://|git@)\S+)");
re!(CLONE, r"git clone (?:--\S+ )*((?:https?://|git@)\S+)");
re!(REMOTE_V, r"(?m)^origin\s+(\S+)\s+\(fetch\)");
re!(NEW_BRANCH, r"\* \[new branch\]\s+(\S+)\s+->");
re!(ON_BRANCH, r"(?m)^On branch (\S+)");
re!(
    COMMIT_LINE,
    r"(?m)^\[([\w./-]+)(?: \(root-commit\))? ([0-9a-f]{7,40})\] (.+)$"
);
re!(
    WORKTREE_ADD,
    r"git worktree add\s+(?:-[fqB]\s+)*(?:-b\s+(\S+)\s+)?(\S+)(?:\s+(?:-b\s+(\S+)))?"
);
re!(GH_PR_CREATE, r"gh pr create");
re!(GH_PR_MERGE, r"gh pr merge\s+(\d+)?");
re!(SHA, r"\b[0-9a-f]{40}\b");

/// Extracts facts from a parsed session. Facts the parser recorded stay first.
pub fn extract(s: &ParsedSession, cfg: &Config, tickets: &[Regex]) -> Vec<Fact> {
    let mut facts = s.facts.clone();

    // Repo strength says how sure we are the session worked *in* that repo:
    // 5 recorded by the agent, 4 the session's own PR, 3 pushed to,
    // 2 `git remote -v`, 1 cloned, 0 only mentioned.
    let repo_from = |facts: &mut Vec<Fact>, url: &str, strength: i32| {
        if let Some(k) = normalize_remote(url) {
            add(
                facts,
                Fact::new("repo", k.clone(), repo_display(&k), "parsed", strength),
            );
        }
    };

    for turn in &s.turns {
        if let Some(b) = &turn.branch {
            add(&mut facts, Fact::new("branch", b, b, "recorded", 1));
        }
        if let Some(c) = &turn.cwd {
            add(&mut facts, Fact::new("worktree", c, c, "recorded", 1));
        }
        let mut text_blobs: Vec<&str> = vec![&turn.user, &turn.assistant];
        for tc in &turn.tools {
            text_blobs.push(&tc.input);
            text_blobs.push(&tc.output);
            for f in &tc.files {
                add(
                    &mut facts,
                    Fact::new("file", f, short_path(f, s.cwd.as_deref()), "recorded", 1),
                );
            }
            let cmd = tc.input.as_str();
            let out = tc.output.as_str();
            let ok = !tc.is_error;
            if GH_PR_CREATE.is_match(cmd) && ok {
                for c in PR_URL.captures_iter(out) {
                    add(&mut facts, pr_fact(&c, 4));
                }
            }
            if let Some(c) = GH_PR_MERGE.captures(cmd)
                && ok
                && let Some(n) = c.get(1)
            {
                add(
                    &mut facts,
                    Fact::new("merged", n.as_str(), n.as_str(), "parsed", 1),
                );
            }
            for c in PR_NEW.captures_iter(out) {
                let repo = format!("github.com/{}/{}", &c[1], &c[2]).to_lowercase();
                add(
                    &mut facts,
                    Fact::new("repo", repo.clone(), repo_display(&repo), "parsed", 3),
                );
                add(&mut facts, Fact::new("pushed", &c[3], &c[3], "parsed", 2));
            }
            for c in PUSH_TO.captures_iter(out) {
                repo_from(&mut facts, &c[1], 3);
            }
            for c in CLONE.captures_iter(cmd) {
                repo_from(&mut facts, &c[1], 1);
            }
            for c in REMOTE_V.captures_iter(out) {
                repo_from(&mut facts, &c[1], 2);
            }
            for c in NEW_BRANCH.captures_iter(out) {
                add(&mut facts, Fact::new("branch", &c[1], &c[1], "parsed", 1));
            }
            for c in ON_BRANCH.captures_iter(out) {
                add(&mut facts, Fact::new("branch", &c[1], &c[1], "parsed", 1));
            }
            for c in COMMIT_LINE.captures_iter(out) {
                add(&mut facts, Fact::new("branch", &c[1], &c[1], "parsed", 1));
                add(
                    &mut facts,
                    Fact::new(
                        "commit",
                        &c[2],
                        format!("{} {}", &c[2], crate::model::clip_chars(&c[3], 100)),
                        "parsed",
                        1,
                    ),
                );
            }
            if let Some(c) = WORKTREE_ADD.captures(cmd) {
                if let Some(p) = c.get(2) {
                    add(
                        &mut facts,
                        Fact::new("worktree", p.as_str(), p.as_str(), "parsed", 1),
                    );
                }
                if let Some(b) = c.get(1).or(c.get(3)) {
                    add(
                        &mut facts,
                        Fact::new("branch", b.as_str(), b.as_str(), "parsed", 1),
                    );
                }
            }
            for m in SHA.find_iter(out) {
                // Full SHAs in output are usually from `git log` or `rev-parse`.
                add(
                    &mut facts,
                    Fact::new("sha", m.as_str(), &m.as_str()[..10], "parsed", 1),
                );
            }
        }
        for blob in text_blobs {
            for c in PR_URL.captures_iter(blob) {
                add(&mut facts, pr_fact(&c, 1));
            }
        }
        // Tickets only from the user's own words: assistant text and tool
        // output are full of look-alikes such as review finding labels.
        add_tickets(&mut facts, &turn.user, cfg, tickets);
    }
    let subjects: Vec<String> = facts
        .iter()
        .filter(|f| f.kind == "commit")
        .map(|f| f.display.clone())
        .collect();
    for s in subjects {
        add_tickets(&mut facts, &s, cfg, tickets);
    }
    // Ticket IDs in branch names (feature/ABC-123-foo) count too.
    let branches: Vec<String> = facts
        .iter()
        .filter(|f| f.kind == "branch")
        .map(|f| f.key.to_uppercase())
        .collect();
    for b in branches {
        add_tickets(&mut facts, &b, cfg, tickets);
    }
    // PRs mark every repo they belong to.
    let pr_repos: Vec<(String, i32)> = facts
        .iter()
        .filter(|f| f.kind == "pr")
        .filter_map(|f| f.key.split_once('#').map(|x| (x.0.to_string(), f.strength)))
        .collect();
    for (r, strength) in pr_repos {
        let key = if r.contains('.') {
            r
        } else {
            format!("github.com/{r}")
        };
        add(
            &mut facts,
            Fact::new(
                "repo",
                key.clone(),
                repo_display(&key),
                "parsed",
                if strength >= 3 { 3 } else { 0 },
            ),
        );
    }
    facts
}

fn add_tickets(facts: &mut Vec<Fact>, text: &str, cfg: &Config, tickets: &[Regex]) {
    for re in tickets {
        for m in re.find_iter(text) {
            let t = m.as_str();
            let prefix = t.split('-').next().unwrap_or("");
            if !cfg.ticket_stoplist.iter().any(|x| x == prefix) {
                add(facts, Fact::new("ticket", t, t, "parsed", 1));
            }
        }
    }
}

fn add(facts: &mut Vec<Fact>, f: Fact) {
    if let Some(old) = facts
        .iter_mut()
        .find(|x| x.kind == f.kind && x.key == f.key)
    {
        old.strength = old.strength.max(f.strength);
        if f.origin == "recorded" {
            old.origin = "recorded";
        }
    } else {
        facts.push(f);
    }
}

fn pr_fact(c: &regex::Captures, strength: i32) -> Fact {
    let host = &c[1];
    let repo = format!("{}/{}", &c[2], &c[3]);
    let key = if host == "github.com" {
        format!("{}#{}", repo.to_lowercase(), &c[4])
    } else {
        format!("{host}/{}#{}", repo.to_lowercase(), &c[4])
    };
    Fact::new("pr", key, format!("{repo}#{}", &c[4]), "parsed", strength)
}

fn short_path(p: &str, cwd: Option<&str>) -> String {
    match cwd.and_then(|c| p.strip_prefix(c)) {
        Some(rest) => rest.trim_start_matches('/').to_string(),
        None => p.to_string(),
    }
}

/// The repo for a cwd when no session recorded a remote: the configured
/// worktree layouts first, then the last path segment.
pub fn repo_from_path(cwd: &str, patterns: &[Regex]) -> (String, String) {
    for re in patterns {
        if let Some(c) = re.captures(cwd)
            && let Some(r) = c.name("repo")
        {
            return (
                format!("path:{}", r.as_str().to_lowercase()),
                r.as_str().to_string(),
            );
        }
    }
    let name = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or(cwd);
    (format!("path:{}", name.to_lowercase()), name.to_string())
}

pub fn compile(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|p| {
            Regex::new(p)
                .map_err(|e| tracing::warn!("bad pattern {p}: {e}"))
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_normalize_to_one_key() {
        for u in [
            "https://github.com/Owner/Repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo",
            "https://x-access-token:abc@github.com/owner/repo/",
        ] {
            assert_eq!(
                normalize_remote(u).as_deref(),
                Some("github.com/owner/repo"),
                "{u}"
            );
        }
        assert_eq!(normalize_remote("/local/path"), None);
    }

    #[test]
    fn commit_and_push_output_yield_entities() {
        let mut s = ParsedSession::default();
        s.turns.push(crate::model::Turn {
            tools: vec![
                crate::model::ToolCall {
                    input: "git commit -m 'fix ordering'".into(),
                    output: "[fix-order 1a2b3c4] Fix ordering in history export\n 1 file changed".into(),
                    ..Default::default()
                },
                crate::model::ToolCall {
                    input: "git push -u origin fix-order".into(),
                    output: "remote: Create a pull request for 'fix-order' on GitHub by visiting:\nremote:      https://github.com/acme/widgets/pull/new/fix-order\nTo github.com:acme/widgets.git\n * [new branch]      fix-order -> fix-order".into(),
                    ..Default::default()
                },
                crate::model::ToolCall {
                    input: "gh pr create --fill".into(),
                    output: "https://github.com/acme/widgets/pull/42".into(),
                    ..Default::default()
                },
            ],
            user: "please fix ABC-123, not UTF-8".into(),
            ..Default::default()
        });
        let cfg = Config::default();
        let f = extract(&s, &cfg, &compile(&cfg.ticket_patterns));
        let has = |k: &str, key: &str| f.iter().find(|x| x.kind == k && x.key == key).cloned();
        assert!(has("repo", "github.com/acme/widgets").is_some());
        assert!(has("branch", "fix-order").is_some());
        assert!(has("commit", "1a2b3c4").is_some());
        assert_eq!(has("pr", "acme/widgets#42").unwrap().strength, 4);
        assert!(has("ticket", "ABC-123").is_some());
        assert!(has("ticket", "UTF-8").is_none());
    }

    #[test]
    fn default_worktree_layouts_name_the_repo() {
        let pats = compile(&crate::config::Config::default().worktree_patterns);
        for (cwd, repo) in [
            ("/home/u/.codex/worktrees/0867/shop", "shop"),
            ("/home/u/.codex/worktrees/0867/shop/src", "shop"),
            ("/home/u/.herdr/worktrees/shop/fix-login", "shop"),
            ("/home/u/conductor/workspaces/shop/nagoya", "shop"),
            ("/home/u/code/shop/.claude/worktrees/fix-login", "shop"),
        ] {
            assert_eq!(repo_from_path(cwd, &pats).1, repo, "{cwd}");
        }
    }
}
