//! Opening external URLs in the user's browser.
//!
//! With no `[browser] open_command` configured, URLs go through the GIO
//! default handler, i.e. the desktop's default browser. A configured command
//! overrides that for taarof only, e.g. `open_command = "helium-browser {url}"`.

/// Turn a `[browser] open_command` template into an argv vector. The template
/// is split on whitespace and `{url}` tokens are substituted; a template with
/// no `{url}` placeholder gets the URL appended as the final argument.
pub(crate) fn build_browser_argv(open_command: &str, url: &str) -> Option<Vec<String>> {
    let tokens: Vec<&str> = open_command.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let mut argv: Vec<String> = tokens
        .iter()
        .map(|token| token.replace("{url}", url))
        .collect();
    if !tokens.iter().any(|token| token.contains("{url}")) {
        argv.push(url.to_string());
    }
    Some(argv)
}

/// Open a URL in the configured browser. Falls back to the system default URI
/// handler when no `[browser] open_command` is set or the spawn fails, so a
/// bad config never leaves links dead.
pub(crate) fn open_url(url: &str) {
    if let Some(command) = crate::config::browser_config().open_command {
        if let Some(argv) = build_browser_argv(&command, url) {
            let (program, args) = argv.split_first().expect("argv is non-empty");
            let mut command = std::process::Command::new(program);
            command.args(args);
            match crate::child_process::spawn_and_reap(&mut command) {
                Ok(_) => return,
                Err(e) => eprintln!(
                    "taarof: failed to launch browser {program:?}: {e}; using system default"
                ),
            }
        }
    }
    if let Err(e) = gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>) {
        eprintln!("taarof: failed to open URL '{url}': {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::build_browser_argv;

    #[test]
    fn substitutes_url_placeholder() {
        let argv = build_browser_argv("helium-browser {url}", "https://example.com").unwrap();
        assert_eq!(argv, vec!["helium-browser", "https://example.com"]);
    }

    #[test]
    fn appends_url_when_template_has_no_placeholder() {
        let argv = build_browser_argv("firefox --new-tab", "https://example.com").unwrap();
        assert_eq!(argv, vec!["firefox", "--new-tab", "https://example.com"]);
    }

    #[test]
    fn rejects_blank_template() {
        assert_eq!(build_browser_argv("   ", "https://example.com"), None);
    }
}
