-- Initial gateway schema.
--
-- Redaction by construction: NO table in this schema may hold terminal
-- content, input bytes, pasted text, bearer tokens, token hashes, local
-- filesystem paths, process identifiers, or Unix socket locations. Audit rows
-- carry metadata only (see `audit_log` below).

-- Paired devices. Public keys and attestation facts are recorded here; the
-- private keys never leave the device's hardware keystore.
CREATE TABLE devices (
    id                 INTEGER PRIMARY KEY,
    -- Stable opaque identifier for the paired device.
    device_uuid        TEXT NOT NULL UNIQUE,
    -- Operator-facing device name shown at pairing confirmation time.
    display_name       TEXT NOT NULL,
    -- SPKI/public-key material for the observe and control keys.
    observe_public_key BLOB NOT NULL,
    control_public_key BLOB,
    -- Verified hardware-attestation facts (app identity, TEE/StrongBox level,
    -- authorization list), stored as JSON text.
    attestation_facts  TEXT,
    -- Highest verified security level, e.g. "strongbox" or "tee".
    security_level     TEXT,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- Non-null once the device is revoked from the owner host.
    revoked_at         TEXT
);

CREATE INDEX idx_devices_active ON devices (revoked_at);

-- Metadata-only audit trail. Columns are intentionally limited to identity,
-- runtime/pane identifiers, action category, byte count, outcome, and time.
CREATE TABLE audit_log (
    id                  INTEGER PRIMARY KEY,
    -- Which paired device acted (nullable for pre-auth events).
    device_uuid         TEXT,
    -- The pinned runtime this gateway serves.
    runtime_instance_id TEXT NOT NULL,
    -- Logical tab/pane identifiers (never filesystem paths or PIDs).
    tab_id              TEXT,
    pane_id             TEXT,
    -- Coarse action category, e.g. "attach", "input", "tab_create".
    action_category     TEXT NOT NULL,
    -- Size of the associated payload; the payload itself is never stored.
    byte_count          INTEGER NOT NULL DEFAULT 0,
    -- "ok", "denied", "rate_limited", "error", etc.
    outcome             TEXT NOT NULL,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_audit_log_created_at ON audit_log (created_at);
CREATE INDEX idx_audit_log_device ON audit_log (device_uuid);
