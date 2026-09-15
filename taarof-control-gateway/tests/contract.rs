//! Protocol conformance tests driven by the language-neutral fixtures in
//! `protocol/fixtures`.
//!
//! These run the gateway's own terminal-frame validation and capability
//! negotiation against the exact fixtures the Android client and any future iOS
//! client are validated against, so a divergence between the gateway and the
//! published protocol is a test failure rather than a silent interop break.
//!
//! Step 1 of the task brief: the negative fixtures must be *rejected* with the
//! error the manifest predicts before any relay wiring exists, and the positive
//! fixtures must validate.

use std::path::{Path, PathBuf};

use serde_json::Value;

use taarof_control_gateway::terminal::{validate_frame, validate_negotiation, NonceLedger};

/// A frozen "now" in 2020. Every fixture deadline is either far in the future
/// (year 2100) or far in the past (year 2000), so this makes freshness
/// deterministic without touching the wall clock.
const NOW_MS: u64 = 1_600_000_000_000;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../protocol/fixtures")
        .canonicalize()
        .expect("protocol fixtures directory must exist")
}

fn read_json(rel: &str) -> Value {
    let path = fixtures_dir().join(rel);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing fixture {}: {e}", path.display()))
}

fn manifest() -> Value {
    read_json("manifest.json")
}

/// Validate a single fixture value by its manifest `type`, threading `nonces`
/// so a multi-frame fixture can prove nonce single-use across frames.
fn run_fixture(fixture_type: &str, value: &Value, nonces: &mut NonceLedger) -> Result<(), String> {
    match fixture_type {
        "terminal_frame" => validate_frame(value, NOW_MS, nonces).map_err(|e| e.to_string()),
        "terminal_frames" => {
            let frames = value
                .as_array()
                .expect("terminal_frames fixture is an array");
            // The error, if any, is whichever frame first rejects — the reused
            // nonce is the second frame, so earlier frames must pass.
            for frame in frames {
                validate_frame(frame, NOW_MS, nonces).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        "negotiation" => validate_negotiation(value).map_err(|e| e.to_string()),
        other => panic!("unknown fixture type {other}"),
    }
}

#[test]
fn positive_fixtures_validate() {
    let manifest = manifest();
    for entry in manifest["positive"].as_array().unwrap() {
        let fixture_type = entry["type"].as_str().unwrap();
        let file = entry["file"].as_str().unwrap();
        let value = read_json(file);
        let mut nonces = NonceLedger::new();
        run_fixture(fixture_type, &value, &mut nonces)
            .unwrap_or_else(|e| panic!("positive fixture {file} must validate, got error: {e}"));
    }
}

#[test]
fn negative_fixtures_are_rejected_with_expected_error() {
    let manifest = manifest();
    for entry in manifest["negative"].as_array().unwrap() {
        let fixture_type = entry["type"].as_str().unwrap();
        let file = entry["file"].as_str().unwrap();
        let expected = entry["expected_error"].as_str().unwrap();
        let value = read_json(file);
        let mut nonces = NonceLedger::new();
        let err = run_fixture(fixture_type, &value, &mut nonces)
            .expect_err(&format!("negative fixture {file} must be rejected"));
        assert!(
            err.contains(expected),
            "fixture {file}: expected error containing {expected:?}, got {err:?}"
        );
    }
}

#[test]
fn a_fresh_nonce_is_single_use_within_a_connection() {
    // Independent of the fixtures: the ledger must reject the second use of any
    // nonce, mirroring the reused-nonce fixture's guarantee.
    let mut nonces = NonceLedger::new();
    assert!(nonces.consume("11111111-1111-4111-8111-111111111111"));
    assert!(!nonces.consume("11111111-1111-4111-8111-111111111111"));
}
