//! Ambient-credential denylist shared by every crate that launches children.
//!
//! taarof and the `agent` launcher both inherit a desktop or shell environment
//! that may carry an Infisical access/service token. Neither may hand that
//! token to a child: consumers that need Infisical mint their own through the
//! machine-identity/broker path. Each crate applies this list at its own child
//! construction seam; keeping one list here stops the two copies drifting.

/// Token-shaped Infisical credentials that must never cross a child-process
/// boundary. Machine-identity inputs are intentionally not listed: explicit
/// consumers may use those to mint their own short-lived token.
pub const AMBIENT_INFISICAL_TOKEN_VARS: &[&str] = &["INFISICAL_TOKEN", "INFISICAL_SERVICE_TOKEN"];

pub fn is_ambient_infisical_token(name: &str) -> bool {
    AMBIENT_INFISICAL_TOKEN_VARS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denylist_names_only_token_shaped_credentials() {
        assert!(is_ambient_infisical_token("INFISICAL_TOKEN"));
        assert!(is_ambient_infisical_token("INFISICAL_SERVICE_TOKEN"));
        for kept in [
            "INFISICAL_URL",
            "INFISICAL_PROJECT_ID",
            "INFISICAL_CLIENT_ID",
            "INFISICAL_CLIENT_SECRET",
        ] {
            assert!(
                !is_ambient_infisical_token(kept),
                "{kept} must stay available"
            );
        }
    }
}
