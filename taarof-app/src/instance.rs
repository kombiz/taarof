use sha2::{Digest, Sha256};

const SESSION_SLUG_PREFIX_MAX: usize = 48;

fn normalize_session_name(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn hex_fallback(raw: &str) -> String {
    raw.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sanitize_component(raw: &str, replacement: char, keep: impl Fn(char) -> bool) -> String {
    let mut normalized = String::with_capacity(raw.len());
    let mut last_was_replacement = false;

    for ch in raw.chars() {
        if keep(ch) {
            normalized.push(ch);
            last_was_replacement = false;
        } else if !last_was_replacement {
            normalized.push(replacement);
            last_was_replacement = true;
        }
    }

    let trimmed = normalized.trim_matches(replacement).to_string();
    if trimmed.is_empty() {
        hex_fallback(raw)
    } else {
        trimmed
    }
}

fn sanitize_file_component(raw: &str) -> String {
    sanitize_component(raw, '-', |ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
    })
    .chars()
    .take(SESSION_SLUG_PREFIX_MAX)
    .collect()
}

fn session_digest(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    // A 128-bit SHA-256 prefix keeps runtime paths and application IDs
    // manageable while making sanitized-name aliasing cryptographically
    // impractical. The CLI implements this exact UTF-8 byte contract.
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn session_storage_key_for(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return "default".to_string();
    }
    format!("{}-{}", sanitize_file_component(raw), session_digest(raw))
}

fn sanitize_app_id_component(raw: &str) -> String {
    let component = sanitize_component(raw, '_', |ch| ch.is_ascii_alphanumeric() || ch == '_');
    match component.chars().next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            component.chars().take(SESSION_SLUG_PREFIX_MAX).collect()
        }
        Some(_) => format!(
            "s_{}",
            component
                .chars()
                .take(SESSION_SLUG_PREFIX_MAX - 2)
                .collect::<String>()
        ),
        None => "session".to_string(),
    }
}

fn application_id_for_session(base: &str, session: Option<&str>) -> String {
    match normalize_session_name(session) {
        Some(name) => format!(
            "{base}.{}_{}",
            sanitize_app_id_component(&name),
            session_digest(&name)
        ),
        None => base.to_string(),
    }
}

pub fn session_name() -> Option<String> {
    let raw = std::env::var("TAAROF_SESSION").ok();
    normalize_session_name(raw.as_deref())
}

pub fn session_slug() -> Option<String> {
    session_name().map(|name| sanitize_file_component(&name))
}

/// Collision-safe filename/registry key for a raw named session. Unlike the
/// display slug, this cannot alias `dev/tools` with `dev tools`.
pub fn session_storage_key() -> Option<String> {
    session_name().map(|name| session_storage_key_for(&name))
}

/// Fixed-size digest used where even a bounded display prefix is unnecessary
/// or platform path limits are especially tight (notably Unix sockets).
pub(crate) fn session_digest_key() -> Option<String> {
    session_name().map(|name| session_digest(&name))
}

pub fn application_id(base: &str) -> String {
    application_id_for_session(base, session_name().as_deref())
}

#[cfg(test)]
mod tests {
    use super::{
        application_id_for_session, normalize_session_name, sanitize_app_id_component,
        sanitize_file_component, session_digest, session_storage_key_for, SESSION_SLUG_PREFIX_MAX,
    };

    #[test]
    fn session_name_trims_empty_values() {
        assert_eq!(normalize_session_name(Some("  ")), None);
        assert_eq!(
            normalize_session_name(Some(" dev ")),
            Some("dev".to_string())
        );
    }

    #[test]
    fn file_component_sanitizes_for_paths() {
        assert_eq!(sanitize_file_component(" dev/tools "), "dev-tools");
        assert_eq!(sanitize_file_component("alpha_beta"), "alpha_beta");
    }

    #[test]
    fn app_id_component_starts_with_a_valid_character() {
        assert_eq!(
            sanitize_app_id_component("123 dev-tools"),
            "s_123_dev_tools"
        );
        assert_eq!(sanitize_app_id_component("dev-tools"), "dev_tools");
    }

    #[test]
    fn application_id_scopes_named_sessions() {
        assert_eq!(
            application_id_for_session("io.github.kombiz.taarof", Some("dev")),
            format!("io.github.kombiz.taarof.dev_{}", session_digest("dev"))
        );
        assert_eq!(
            application_id_for_session("io.github.kombiz.taarof", Some("123 dev")),
            format!(
                "io.github.kombiz.taarof.s_123_dev_{}",
                session_digest("123 dev")
            )
        );
        assert_eq!(
            application_id_for_session("io.github.kombiz.taarof", None),
            "io.github.kombiz.taarof"
        );
        assert_ne!(
            application_id_for_session("io.github.kombiz.taarof", Some("a?={$}%[)^,b")),
            application_id_for_session("io.github.kombiz.taarof", Some(r"a#\,|~^@:?{b"))
        );
    }

    #[test]
    fn storage_key_prevents_slug_collisions() {
        assert_ne!(
            session_storage_key_for("dev/tools"),
            session_storage_key_for("dev tools")
        );
        assert!(session_storage_key_for("dev/tools").starts_with("dev-tools-"));
        assert_ne!(
            session_storage_key_for("a?={$}%[)^,b"),
            session_storage_key_for(r"a#\,|~^@:?{b")
        );
        assert_eq!(
            session_storage_key_for("a?={$}%[)^,b"),
            "a-b-625bb59eb804ec5479e63735527b8d3b"
        );
        assert_eq!(
            session_storage_key_for(r"a#\,|~^@:?{b"),
            "a-b-586c365797efde941acc38aae9d0f2f9"
        );
    }

    #[test]
    fn storage_key_trims_and_hashes_unicode_bytes_consistently() {
        let cat = session_storage_key_for("🐈");
        assert_eq!(cat, "f09f9088-65607de0a69ab2c3979891a88d8ce774");
        assert_eq!(cat, session_storage_key_for("  🐈  "));
        assert!(cat.starts_with("f09f9088-"));
        assert_eq!(session_storage_key_for("   "), "default");
    }

    #[test]
    fn very_long_names_have_bounded_keys_and_valid_application_ids() {
        let ascii = "A".repeat(1_000);
        let unicode = "界".repeat(1_000);
        assert_eq!(
            session_storage_key_for(&ascii),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-c2e686823489ced2017f6059b8b23931"
        );
        assert_eq!(
            session_storage_key_for(&unicode),
            "e7958ce7958ce7958ce7958ce7958ce7958ce7958ce7958c-a9df9287ae828284ce75d277cad80a38"
        );
        for raw in [&ascii, &unicode] {
            let key = session_storage_key_for(raw);
            assert!(key.is_ascii());
            assert!(key.len() <= SESSION_SLUG_PREFIX_MAX + 1 + 32);
            for component in [
                format!("session-{key}.json"),
                format!("work-ledger-{key}.json"),
                format!("taarof-current-{key}.json"),
                format!("diagnostics-{key}.jsonl"),
                format!("history-{key}.sqlite3"),
                format!("history-{key}.sqlite3-wal"),
                format!("history-{key}.sqlite3-shm"),
            ] {
                assert!(component.is_ascii());
                assert!(component.len() <= 255);
            }

            let app_id = application_id_for_session("io.github.kombiz.taarof", Some(raw));
            assert!(app_id.is_ascii());
            assert!(app_id.len() <= 255);
            assert!(app_id.split('.').all(|component| {
                !component.is_empty()
                    && component
                        .chars()
                        .next()
                        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                    && component
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
            }));
        }
    }
}
