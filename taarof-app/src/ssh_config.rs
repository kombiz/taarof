//! `~/.ssh/config` as the canonical source of SSH connection candidates.
//!
//! Interpretation stops at alias names: the connection argv stays `ssh <alias>`
//! so ssh itself resolves `HostName`/`User`/`Port`/`ProxyJump`. `[hosts.*]`
//! entries in config.toml remain optional tuning and win on name collision.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::host::HostConfig;

/// Include recursion is bounded rather than cycle-tracked: a self-including
/// config must terminate, and 8 levels is far past any hand-written setup.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Candidate lists are rebuilt on every palette keystroke, so the parse result
/// is memoised briefly. This is a TTL, not a watcher: the file is still re-read
/// on the next listing after it expires, and no poll loop is added.
const ALIAS_CACHE_TTL: Duration = Duration::from_secs(5);

/// Where a resolved host came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSource {
    /// A `[hosts.<name>]` entry in config.toml — carries tuning.
    ConfigToml,
    /// A concrete `Host` alias in `~/.ssh/config`.
    SshConfig,
}

/// One offered connection target.
#[derive(Clone, Debug)]
pub struct HostCandidate {
    pub name: String,
    pub source: HostSource,
    pub config: HostConfig,
}

/// Filesystem context for parsing: `base_dir` resolves relative `Include`
/// paths, `home_dir` expands a leading `~`.
struct ParseContext {
    base_dir: PathBuf,
    home_dir: PathBuf,
}

/// Split an ssh_config line into its keyword and the rest of the arguments.
///
/// OpenSSH accepts either whitespace or `=` between a keyword and its value.
fn split_directive(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    let end = line.find(|ch: char| ch.is_whitespace() || ch == '=')?;
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '=');
    Some((keyword.to_ascii_lowercase(), rest))
}

/// Strip the surrounding quotes OpenSSH allows around a token.
fn unquote(token: &str) -> &str {
    token
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(token)
}

/// A `Host` pattern names one concrete alias only when it matches itself
/// literally: wildcards and negations describe sets, not destinations.
fn is_concrete_alias(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.starts_with('!')
        && !pattern.contains('*')
        && !pattern.contains('?')
        && !pattern.chars().any(char::is_control)
}

/// Match a file name against an `Include` pattern supporting `*` and `?`.
fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some(b'*') => {
            glob_match(&pattern[1..], name) || (!name.is_empty() && glob_match(pattern, &name[1..]))
        }
        Some(b'?') => !name.is_empty() && glob_match(&pattern[1..], &name[1..]),
        Some(byte) => name.first() == Some(byte) && glob_match(&pattern[1..], &name[1..]),
    }
}

/// Resolve one `Include` token to the concrete files it names.
///
/// `~` expands against the context's home directory, relative paths against the
/// including file's directory (`~/.ssh` for the user config), and a glob in the
/// final component is expanded in sorted order for a deterministic candidate
/// list.
fn include_paths(token: &str, ctx: &ParseContext) -> Vec<PathBuf> {
    let expanded = match token.strip_prefix("~/") {
        Some(rest) => ctx.home_dir.join(rest),
        None if token == "~" => ctx.home_dir.clone(),
        None => {
            let path = Path::new(token);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                ctx.base_dir.join(path)
            }
        }
    };

    let Some(file_name) = expanded.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !file_name.contains('*') && !file_name.contains('?') {
        return vec![expanded];
    }

    let Some(parent) = expanded.parent() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| glob_match(file_name.as_bytes(), name.as_bytes()))
        })
        .map(|entry| entry.path())
        .collect();
    matches.sort();
    matches
}

fn collect_host_aliases(path: &Path, ctx: &ParseContext, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };

    // `Match` blocks are conditional and never contribute a destination, so
    // everything between a `Match` line and the next `Host` line — including any
    // `Include` — is skipped.
    let mut in_match_block = false;
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((keyword, rest)) = split_directive(line) else {
            continue;
        };
        match keyword.as_str() {
            "host" => {
                in_match_block = false;
                for pattern in rest.split_whitespace().map(unquote) {
                    if is_concrete_alias(pattern) && !out.iter().any(|seen| seen == pattern) {
                        out.push(pattern.to_string());
                    }
                }
            }
            "match" => in_match_block = true,
            "include" if !in_match_block => {
                for token in rest.split_whitespace().map(unquote) {
                    for included in include_paths(token, ctx) {
                        collect_host_aliases(&included, ctx, depth + 1, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_host_aliases_at(path: &Path, ctx: &ParseContext) -> Vec<String> {
    let mut aliases = Vec::new();
    collect_host_aliases(path, ctx, 0, &mut aliases);
    aliases
}

/// Turn a concrete alias into a connection candidate.
///
/// The argv stays `ssh <alias>` — ssh resolves user, port, and jump host — so an
/// alias that would not survive SSH identity validation is dropped rather than
/// handed to a child process.
fn host_config_for_alias(alias: &str) -> Option<HostConfig> {
    crate::projects::validate_ssh_identity(alias, alias).ok()?;
    Some(HostConfig {
        name: alias.to_string(),
        ssh_target: Some(alias.to_string()),
        ..HostConfig::default()
    })
}

fn merge_candidates(hosts: Vec<HostConfig>, aliases: &[String]) -> Vec<HostCandidate> {
    let mut candidates: Vec<HostCandidate> = hosts
        .into_iter()
        .map(|config| HostCandidate {
            name: config.name.clone(),
            source: HostSource::ConfigToml,
            config,
        })
        .collect();
    for alias in aliases {
        if candidates.iter().any(|candidate| candidate.name == *alias) {
            continue;
        }
        if let Some(config) = host_config_for_alias(alias) {
            candidates.push(HostCandidate {
                name: alias.clone(),
                source: HostSource::SshConfig,
                config,
            });
        }
    }
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    candidates
}

fn resolve_with(
    toml_host: Option<HostConfig>,
    aliases: &[String],
    name: &str,
) -> Result<HostConfig, String> {
    if let Some(config) = toml_host {
        return Ok(config);
    }
    aliases
        .iter()
        .find(|alias| *alias == name)
        .and_then(|alias| host_config_for_alias(alias))
        .ok_or_else(|| format!("unknown host: {name}"))
}

fn ssh_config_path() -> Option<(PathBuf, ParseContext)> {
    let home = dirs::home_dir()?;
    let base_dir = home.join(".ssh");
    let path = base_dir.join("config");
    Some((
        path,
        ParseContext {
            base_dir,
            home_dir: home,
        },
    ))
}

thread_local! {
    static ALIAS_CACHE: RefCell<Option<(Instant, Vec<String>)>> = const { RefCell::new(None) };
}

/// Concrete `Host` aliases from `~/.ssh/config`, memoised for [`ALIAS_CACHE_TTL`].
fn host_aliases() -> Vec<String> {
    ALIAS_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((read_at, aliases)) = cache.as_ref() {
            if read_at.elapsed() < ALIAS_CACHE_TTL {
                return aliases.clone();
            }
        }
        let aliases = ssh_config_path()
            .map(|(path, ctx)| parse_host_aliases_at(&path, &ctx))
            .unwrap_or_default();
        *cache = Some((Instant::now(), aliases.clone()));
        aliases
    })
}

/// Every connection candidate offered by the dialog and the palette:
/// `[hosts.*]` remote entries merged with `~/.ssh/config` aliases, config.toml
/// winning on name collision because it carries the tuning.
pub fn candidates() -> Vec<HostCandidate> {
    merge_candidates(crate::config::remote_hosts(), &host_aliases())
}

/// The single host-resolution seam shared by the tmux-tab dialog, the palette,
/// and the socket `create-tmux-tab` handler.
pub fn resolve(name: &str) -> Result<HostConfig, String> {
    resolve_with(crate::config::host_config(name), &host_aliases(), name)
}

/// Resolve a free-text `user@host` target typed into the tmux-tab dialog.
pub fn resolve_free_text(target: &str) -> Result<HostConfig, String> {
    let target = target.trim();
    crate::projects::validate_ssh_identity(target, target)?;
    Ok(HostConfig {
        name: target.to_string(),
        ssh_target: Some(target.to_string()),
        ..HostConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FixtureDir(PathBuf);

    impl FixtureDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "taarof-ssh-config-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create fixture dir");
            Self(dir)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture parent");
            }
            fs::write(&path, contents).expect("write fixture");
            path
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.0.join(relative)
        }
    }

    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_parse_host_aliases_skips_wildcards_and_negations() {
        let fixture = FixtureDir::new("wildcards");
        let config = fixture.write(
            "ssh/config",
            "\
Host *
    ServerAliveInterval 60

Host devbox build.ts
    HostName 10.0.0.4
    User builder

Host *.internal
Host web?
Host !secret prod
# Host commented-out
Host trailing # inline-comment
",
        );
        let ctx = ParseContext {
            base_dir: fixture.path("ssh"),
            home_dir: fixture.path("home"),
        };

        let aliases = parse_host_aliases_at(&config, &ctx);

        assert_eq!(
            aliases,
            vec![
                "devbox".to_string(),
                "build.ts".to_string(),
                "prod".to_string(),
                "trailing".to_string()
            ]
        );
    }

    #[test]
    fn test_parse_follows_include_directives() {
        let fixture = FixtureDir::new("includes");
        let config = fixture.write(
            "ssh/config",
            "\
Include extra/*.conf
Include ~/tilde.conf
Host main
",
        );
        fixture.write("ssh/extra/a.conf", "Host alpha\n    HostName 10.0.0.1\n");
        fixture.write("ssh/extra/b.conf", "Host beta\n");
        fixture.write("ssh/extra/ignored.txt", "Host ignored\n");
        fixture.write("home/tilde.conf", "Host tilde-host\n");
        let ctx = ParseContext {
            base_dir: fixture.path("ssh"),
            home_dir: fixture.path("home"),
        };

        let aliases = parse_host_aliases_at(&config, &ctx);

        assert!(aliases.contains(&"alpha".to_string()), "{aliases:?}");
        assert!(aliases.contains(&"beta".to_string()), "{aliases:?}");
        assert!(aliases.contains(&"tilde-host".to_string()), "{aliases:?}");
        assert!(aliases.contains(&"main".to_string()), "{aliases:?}");
        assert!(!aliases.contains(&"ignored".to_string()), "{aliases:?}");
    }

    #[test]
    fn test_parse_follows_include_directives_stops_at_depth_limit() {
        let fixture = FixtureDir::new("include-cycle");
        let config = fixture.write("ssh/config", "Include config\nHost only\n");
        let ctx = ParseContext {
            base_dir: fixture.path("ssh"),
            home_dir: fixture.path("home"),
        };

        // A self-including config must terminate rather than recurse forever.
        let aliases = parse_host_aliases_at(&config, &ctx);

        assert!(aliases.contains(&"only".to_string()), "{aliases:?}");
    }

    #[test]
    fn test_parse_skips_match_blocks() {
        let fixture = FixtureDir::new("match");
        let config = fixture.write(
            "ssh/config",
            "\
Host keep
    HostName 1.2.3.4

Match host jump
    Include match-only.conf
    User someone

Host after-match
",
        );
        fixture.write("ssh/match-only.conf", "Host should-not-appear\n");
        let ctx = ParseContext {
            base_dir: fixture.path("ssh"),
            home_dir: fixture.path("home"),
        };

        let aliases = parse_host_aliases_at(&config, &ctx);

        assert_eq!(
            aliases,
            vec!["keep".to_string(), "after-match".to_string()],
            "Match blocks and their Includes must not contribute candidates"
        );
    }

    #[test]
    fn test_resolve_host_prefers_hosts_toml_entry_over_ssh_config_alias() {
        let aliases = vec!["devbox".to_string(), "alias-only".to_string()];
        let tuned = HostConfig {
            name: "devbox".to_string(),
            ssh_target: Some("builder@devbox.ts".to_string()),
            max_sessions: 3,
            idle_detach_minutes: 15,
            ..HostConfig::default()
        };

        let resolved =
            resolve_with(Some(tuned.clone()), &aliases, "devbox").expect("config.toml host wins");
        assert_eq!(resolved.ssh_target.as_deref(), Some("builder@devbox.ts"));
        assert_eq!(resolved.max_sessions, 3);
        assert_eq!(resolved.idle_detach_minutes, 15);

        // An alias with no `[hosts.*]` entry connects as plain `ssh <alias>`.
        let alias_only = resolve_with(None, &aliases, "alias-only").expect("alias resolves");
        assert_eq!(alias_only.name, "alias-only");
        assert_eq!(alias_only.ssh_target.as_deref(), Some("alias-only"));
        assert_eq!(
            alias_only.max_sessions,
            HostConfig::default().max_sessions,
            "alias candidates carry default tuning"
        );

        let candidates = merge_candidates(vec![tuned], &aliases);
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alias-only", "devbox"]);
        let devbox = candidates
            .iter()
            .find(|c| c.name == "devbox")
            .expect("devbox candidate");
        assert_eq!(devbox.source, HostSource::ConfigToml);
        assert_eq!(
            devbox.config.ssh_target.as_deref(),
            Some("builder@devbox.ts")
        );
        let alias = candidates
            .iter()
            .find(|c| c.name == "alias-only")
            .expect("alias candidate");
        assert_eq!(alias.source, HostSource::SshConfig);
    }

    #[test]
    fn test_resolve_host_unknown_name_errors() {
        let aliases = vec!["devbox".to_string()];

        let error = resolve_with(None, &aliases, "missing").expect_err("unknown host errors");
        assert_eq!(error, "unknown host: missing");

        // Aliases that would not survive SSH identity validation are not
        // resolvable either — an option-shaped alias must never become argv.
        let hostile = vec!["-oProxyCommand=curl".to_string()];
        assert!(resolve_with(None, &hostile, "-oProxyCommand=curl").is_err());
        assert!(merge_candidates(Vec::new(), &hostile).is_empty());
    }

    #[test]
    fn test_free_text_target_rejected_by_identity_validation() {
        let accepted = resolve_free_text(" builder@devbox.ts ").expect("valid target accepted");
        assert_eq!(accepted.name, "builder@devbox.ts");
        assert_eq!(accepted.ssh_target.as_deref(), Some("builder@devbox.ts"));

        assert!(resolve_free_text("-oProxyCommand=curl evil").is_err());
        assert!(resolve_free_text("devbox; rm -rf /").is_err());
        assert!(resolve_free_text("dev box").is_err());
        assert!(resolve_free_text("   ").is_err());
    }
}
