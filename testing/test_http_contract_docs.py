#!/usr/bin/env python3
"""Fail-closed parity checks for the shipped local HTTP control surface.

The protocol directory also contains the staged remote protocol.  Only OpenAPI
operations explicitly marked ``loopback-http`` describe the Axum router in
``taarof-app/src/http.rs``; conflating the two surfaces would make either the
runtime or the staged protocol lie.
"""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
HTTP_SOURCE = REPO_ROOT / "taarof-app/src/http.rs"
SOCKET_SOURCE = REPO_ROOT / "taarof-app/src/socket.rs"
PROTOCOL_SOURCE = REPO_ROOT / "taarof-app/src/socket/protocol.rs"
PANE_ATTACH_SOURCE = REPO_ROOT / "taarof-app/src/http/pane_attach.rs"
PTY_SOURCE = REPO_ROOT / "taarof-app/src/http/pty.rs"
AUTH_SOURCE = REPO_ROOT / "taarof-app/src/http/auth.rs"
DOCS_SOURCE = REPO_ROOT / "docs/local-query-api.md"
OPENAPI_SOURCE = REPO_ROOT / "protocol/openapi.yaml"
LOOPBACK_HTTP_SURFACE = "loopback-http"
ROUTE_METHODS = ("get", "post", "put", "patch", "delete", "head", "options")

EXPECTED_ROUTE_HANDLERS = {
    ("GET", "/health"): "health",
    ("GET", "/api/v1/runtime-identity"): "get_runtime_identity",
    ("GET", "/api/v1/state"): "get_state",
    ("GET", "/api/v1/agent-sessions"): "get_agent_sessions",
    ("GET", "/api/v1/sessions"): "get_sessions",
    ("GET", "/api/v1/workspaces"): "get_workspaces",
    ("GET", "/api/v1/tabs"): "get_tabs",
    ("GET", "/api/v1/panes"): "get_panes",
    ("GET", "/api/v1/file-preview"): "file_preview",
    ("GET", "/api/v1/file-stat"): "file_stat",
    ("POST", "/api/v1/control/send-keys"): "control_send_keys",
    ("POST", "/api/v1/control/run-in-pane"): "control_run_in_pane",
    ("POST", "/api/v1/control/switch-tab"): "control_switch_tab",
    ("POST", "/api/v1/control/create-tab"): "control_create_tab",
    ("POST", "/api/v1/control/split-pane"): "control_split_pane",
    ("GET", "/api/v1/tabs/{tab_id}/panes/{pane_id}/attach"): "tab_pane_attach_ws",
    ("GET", "/api/v1/tabs/{tab_id}/panes/{pane_id}/control/ws"): "tab_pane_attach_control_ws",
    ("GET", "/api/v1/tabs/{tab_id}/panes/{pane_id}/pty/ws"): "tab_pane_pty_ws",
    ("GET", "/api/v1/panes/{pane_id}/attach"): "pane_attach_ws",
    ("GET", "/api/v1/events"): "get_events",
    ("GET", "/api/v1/history"): "get_history",
    ("GET", "/api/v1/events/ws"): "events_ws",
}

CONTROL_RESPONSE_STATUSES = {"200", "400", "401", "403", "415", "422", "500", "503", "504"}
PANE_WS_RESPONSE_STATUSES = {"101", "400", "401", "404", "409", "429", "500", "503", "504"}
LOOPBACK_SUCCESS_RESPONSE_SCHEMAS = {
    ("GET", "/health"): "LoopbackHealthResponse",
    ("GET", "/api/v1/runtime-identity"): "LoopbackRuntimeIdentityResponse",
    ("GET", "/api/v1/state"): "LoopbackStateResponse",
    ("GET", "/api/v1/agent-sessions"): "LoopbackAgentSessionsResponse",
    ("GET", "/api/v1/sessions"): "LoopbackSessionsResponse",
    ("GET", "/api/v1/workspaces"): "LoopbackWorkspacesResponse",
    ("GET", "/api/v1/tabs"): "LoopbackTabsResponse",
    ("GET", "/api/v1/panes"): "LoopbackPanesResponse",
    ("GET", "/api/v1/file-preview"): "LoopbackFilePreviewResponse",
    ("GET", "/api/v1/file-stat"): "LoopbackFileStatResponse",
    ("GET", "/api/v1/events"): "LoopbackEventsResponse",
    ("GET", "/api/v1/history"): "LoopbackHistoryResponse",
    ("POST", "/api/v1/control/send-keys"): "LoopbackSendKeysResponse",
    ("POST", "/api/v1/control/run-in-pane"): "LoopbackRunInPaneResponse",
    ("POST", "/api/v1/control/switch-tab"): "LoopbackSwitchTabResponse",
    ("POST", "/api/v1/control/create-tab"): "LoopbackCreateTabResponse",
    ("POST", "/api/v1/control/split-pane"): "LoopbackSplitPaneResponse",
}
LOOPBACK_RESPONSE_DATA_SCHEMAS = {
    "LoopbackStateResponse": "LoopbackStateSnapshot",
    "LoopbackAgentSessionsResponse": "LoopbackAgentSessionsSnapshot",
    "LoopbackSessionsResponse": "LoopbackSessionsSnapshot",
    "LoopbackWorkspacesResponse": ("array", "LoopbackWorkspace"),
    "LoopbackTabsResponse": ("array", "LoopbackTabProjection"),
    "LoopbackPanesResponse": ("array", "LoopbackPaneProjection"),
    "LoopbackFilePreviewResponse": "LoopbackFilePreview",
    "LoopbackFileStatResponse": "LoopbackFileStat",
    "LoopbackEventsResponse": "LoopbackEventsPage",
    "LoopbackHistoryResponse": "LoopbackHistoryPage",
    "LoopbackSendKeysResponse": "LoopbackSocketAcknowledgement",
    "LoopbackRunInPaneResponse": "LoopbackSocketAcknowledgement",
    "LoopbackSwitchTabResponse": "LoopbackSocketAcknowledgement",
    "LoopbackCreateTabResponse": "LoopbackSocketCreatedTab",
    "LoopbackSplitPaneResponse": "LoopbackSocketCreatedPane",
}
EXPECTED_RESPONSE_STATUSES = {
    ("GET", "/health"): {"200", "500", "503", "504"},
    ("GET", "/api/v1/runtime-identity"): {"200", "401"},
    ("GET", "/api/v1/state"): {"200", "401", "500", "503", "504"},
    ("GET", "/api/v1/agent-sessions"): {"200", "400", "401", "500", "503", "504"},
    ("GET", "/api/v1/sessions"): {"200", "401", "500", "503", "504"},
    ("GET", "/api/v1/workspaces"): {"200", "401", "500", "503", "504"},
    ("GET", "/api/v1/tabs"): {"200", "401", "500", "503", "504"},
    ("GET", "/api/v1/panes"): {"200", "401", "500", "503", "504"},
    ("GET", "/api/v1/file-preview"): {"200", "400", "401", "403", "404", "413", "415", "500", "503", "504"},
    ("GET", "/api/v1/file-stat"): {"200", "400", "401", "403", "404", "413", "415", "500", "503", "504"},
    ("GET", "/api/v1/tabs/{tab_id}/panes/{pane_id}/attach"): PANE_WS_RESPONSE_STATUSES,
    ("GET", "/api/v1/tabs/{tab_id}/panes/{pane_id}/control/ws"): PANE_WS_RESPONSE_STATUSES | {"403"},
    ("GET", "/api/v1/tabs/{tab_id}/panes/{pane_id}/pty/ws"): PANE_WS_RESPONSE_STATUSES | {"403"},
    ("GET", "/api/v1/panes/{pane_id}/attach"): PANE_WS_RESPONSE_STATUSES,
    ("GET", "/api/v1/events"): {"200", "400", "401", "500", "503", "504"},
    ("GET", "/api/v1/history"): {"200", "400", "401", "500", "503"},
    ("GET", "/api/v1/events/ws"): {"101", "400", "401"},
}
for _route, _handler in EXPECTED_ROUTE_HANDLERS.items():
    if _route[0] == "POST":
        EXPECTED_RESPONSE_STATUSES[_route] = CONTROL_RESPONSE_STATUSES


def item_body(source: str, marker: str) -> str:
    """Extract one braced Rust item without depending on a Rust parser."""
    start = source.index(marker)
    opening = source.index("{", start)
    depth = 0
    for index in range(opening, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[opening + 1 : index]
    raise AssertionError(f"unterminated Rust item: {marker}")


def rust_enum_variants(source: str, enum_name: str) -> set[str]:
    body = item_body(source, f"enum {enum_name}")
    return set(re.findall(r"^    ([A-Z][A-Za-z0-9_]*)\s*(?:\{|,)", body, re.MULTILINE))


def rust_struct_variant_fields(source: str, enum_name: str) -> dict[str, set[str]]:
    body = item_body(source, f"enum {enum_name}")
    fields: dict[str, set[str]] = {}
    for variant in rust_enum_variants(source, enum_name):
        if not re.search(rf"^    {variant}\s*\{{", body, re.MULTILINE):
            continue
        variant_body = item_body(body, f"    {variant}")
        fields[variant] = set(
            re.findall(r"^        ([a-z][A-Za-z0-9_]*)\s*:", variant_body, re.MULTILINE)
        )
    return fields


def named_fields(source: str) -> dict[str, str]:
    fields: dict[str, str] = {}
    for entry in source.split(","):
        entry = entry.strip()
        if not entry:
            continue
        name, separator, value = entry.partition(":")
        if not re.fullmatch(r"[a-z][A-Za-z0-9_]*", name.strip()):
            continue
        fields[name.strip()] = value.strip() if separator else name.strip()
    return fields


def closing_parenthesis(source: str, opening: int) -> int:
    depth = 0
    for index in range(opening, len(source)):
        if source[index] == "(":
            depth += 1
        elif source[index] == ")":
            depth -= 1
            if depth == 0:
                return index
    raise AssertionError("unterminated router route")


def router_routes(http_source: str) -> set[tuple[str, str]]:
    body = item_body(http_source, "fn build_router_with_state")
    route_start = 0
    routes: set[tuple[str, str]] = set()
    route_pattern = re.compile(r'\.route\(\s*"([^"]+)"\s*,')
    method_pattern = re.compile(rf"\b({'|'.join(ROUTE_METHODS)})\s*\(")
    while match := route_pattern.search(body, route_start):
        opening = body.index("(", match.start())
        closing = closing_parenthesis(body, opening)
        methods = method_pattern.findall(body[match.end() : closing])
        if methods:
            routes.update((method.upper(), match.group(1)) for method in methods)
        else:
            routes.add(("UNKNOWN", match.group(1)))
        route_start = closing + 1
    return routes


def router_bindings(http_source: str) -> dict[tuple[str, str], str]:
    """Read the registered Axum handler, not just the method/path marker."""
    body = item_body(http_source, "fn build_router_with_state")
    bindings: dict[tuple[str, str], str] = {}
    route_start = 0
    route_pattern = re.compile(r'\.route\(\s*"([^"]+)"\s*,')
    binding_pattern = re.compile(r"\b(" + "|".join(ROUTE_METHODS) + r")\s*\(\s*([a-z][A-Za-z0-9_]*)\s*\)")
    while match := route_pattern.search(body, route_start):
        opening = body.index("(", match.start())
        closing = closing_parenthesis(body, opening)
        for method, handler in binding_pattern.findall(body[match.end() : closing]):
            route = (method.upper(), match.group(1))
            if route in bindings:
                raise AssertionError(f"router registers {route} more than once")
            bindings[route] = handler
        route_start = closing + 1
    return bindings


def resolve_ref(document: dict, value: dict) -> dict:
    while "$ref" in value:
        ref = value["$ref"]
        if not ref.startswith("#/"):
            raise AssertionError(f"local loopback contract must use an internal component ref: {ref}")
        target: object = document
        for part in ref[2:].split("/"):
            target = target[part]  # type: ignore[index]
        if not isinstance(target, dict):
            raise AssertionError(f"component ref does not resolve to an object: {ref}")
        value = target
    return value


def rust_function(source: str, name: str) -> tuple[str, str]:
    match = re.search(rf"\b(?:pub\([^)]*\)\s+)?(?:pub\s+)?(?:async\s+)?fn\s+{name}\s*\(", source)
    if match is None:
        raise AssertionError(f"missing Rust function {name}")
    opening = source.index("{", match.end())
    return source[match.start() : opening], item_body(source, source[match.start() : opening])


def source_for_handler(name: str, http: str, pane_attach: str, pty: str) -> str:
    for source in (http, pane_attach, pty):
        if re.search(rf"\bfn\s+{name}\s*\(", source):
            return source
    raise AssertionError(f"registered handler {name} is absent from the HTTP modules")


def rust_struct_fields(source: str, name: str) -> dict[str, str]:
    body = item_body(source, f"struct {name}")
    fields: dict[str, str] = {}
    for field, ty in re.findall(r"^\s*(?:pub\([^)]*\)\s+)?([a-z][A-Za-z0-9_]*)\s*:\s*([^,\n]+)", body, re.MULTILINE):
        fields[field] = ty.strip()
    return fields


def rust_struct_fields_any(name: str, *sources: str) -> dict[str, str]:
    for source in sources:
        if f"struct {name}" in source:
            return rust_struct_fields(source, name)
    raise AssertionError(f"missing Rust struct {name}")


def rust_type_kind(ty: str) -> tuple[str, bool]:
    optional = ty.startswith("Option<") and ty.endswith(">")
    inner = ty[7:-1].strip() if optional else ty
    if inner == "String":
        return "string", optional
    if inner == "crate::history::HistoryOrder":
        # This non-Option field carries #[serde(default)], so omission is valid.
        return "string", True
    if inner in {"u32", "u64", "usize"}:
        return "integer", optional
    raise AssertionError(f"extend contract parser for Rust field type {ty}")


def schema_types(document: dict, schema: dict) -> set[str]:
    schema = resolve_ref(document, schema)
    raw = schema.get("type")
    if isinstance(raw, str):
        return {raw}
    if isinstance(raw, list) and all(isinstance(entry, str) for entry in raw):
        return set(raw)
    return set()


def operation_parameters(document: dict, operation: dict) -> dict[tuple[str, str], dict]:
    parameters: dict[tuple[str, str], dict] = {}
    for raw in operation.get("parameters", []):
        parameter = resolve_ref(document, raw)
        key = (parameter.get("in"), parameter.get("name"))
        if not all(isinstance(value, str) for value in key):
            raise AssertionError(f"invalid OpenAPI parameter {parameter}")
        parameters[key] = parameter
    return parameters


def operation_response(document: dict, operation: dict, status: str) -> dict:
    return resolve_ref(document, operation["responses"][status])


def schema_has_json_content(document: dict, response: dict) -> bool:
    return "application/json" in resolve_ref(document, response).get("content", {})


def loopback_response_schema_errors(openapi: dict) -> list[str]:
    """Require a typed 200 envelope for every concrete JSON runtime result."""
    _, operations = openapi_loopback_routes(openapi)
    schemas = openapi["components"]["schemas"]
    errors: list[str] = []
    for route, response_schema_name in LOOPBACK_SUCCESS_RESPONSE_SCHEMAS.items():
        operation = operations.get(route)
        if operation is None:
            errors.append(f"{route} has no documented loopback operation")
            continue
        resolved_response = operation_response(openapi, operation, "200")
        json_schema = resolved_response.get("content", {}).get("application/json", {}).get("schema")
        expected_ref = f"#/components/schemas/{response_schema_name}"
        if json_schema != {"$ref": expected_ref}:
            errors.append(f"{route} 200 must use {response_schema_name}, not a shared or generic response schema")
            continue
        if response_schema_name not in schemas:
            errors.append(f"missing response schema {response_schema_name}")
            continue
        envelope = schemas[response_schema_name]
        if response_schema_name not in LOOPBACK_RESPONSE_DATA_SCHEMAS:
            continue
        data = envelope.get("properties", {}).get("data")
        if not isinstance(data, dict) or not data:
            errors.append(f"{response_schema_name} has unconstrained data")
            continue
        expected_data_schema = LOOPBACK_RESPONSE_DATA_SCHEMAS[response_schema_name]
        if isinstance(expected_data_schema, tuple):
            kind, item_schema = expected_data_schema
            if data.get("type") != kind or data.get("items") != {"$ref": f"#/components/schemas/{item_schema}"}:
                errors.append(f"{response_schema_name} data does not use {kind} items of {item_schema}")
        elif data != {"$ref": f"#/components/schemas/{expected_data_schema}"}:
            errors.append(f"{response_schema_name} data does not use {expected_data_schema}")
        if envelope.get("required") != ["ok", "data"] or envelope.get("additionalProperties") is not False:
            errors.append(f"{response_schema_name} is not a closed success envelope")
    state = schemas.get("LoopbackStateSnapshot", {})
    if not {"health", "diagnostics", "workspaces", "history"} <= set(state.get("properties", {})):
        errors.append("state response no longer constrains health, diagnostics, workspaces, and history")
    for name, fields in {
        "LoopbackHealthSnapshot": {"state", "components", "events", "update"},
        "LoopbackDiagnostics": {"retention", "counters", "recent"},
        "LoopbackHistoryPage": {"filters", "records", "truncated"},
        "LoopbackEventsPage": {"events", "gap", "resnapshot_required"},
        "LoopbackSocketCreatedTab": {"ok", "tab_id"},
        "LoopbackSocketCreatedPane": {"ok", "pane_id"},
    }.items():
        if not fields <= set(schemas.get(name, {}).get("properties", {})):
            errors.append(f"{name} no longer constrains its documented fields")
    return errors


def canonical_http_claim_errors(docs: str) -> list[str]:
    """Reject false present-tense claims in the shipped HTTP reference only."""
    normalized = " ".join(docs.split())
    errors: list[str] = []
    if "existing read-only HTTP surface" in normalized:
        errors.append("local-query-api still calls the current HTTP surface read-only")
    if re.search(r"\b(?:the )?HTTP API is read-only(?:[.;]|$)", normalized, re.IGNORECASE):
        errors.append("local-query-api claims the HTTP API has no mutating subset")
    if "data is the existing socket command response" in normalized:
        errors.append("local-query-api leaves control success data untyped")
    return errors


def documented_routes(docs: str) -> tuple[set[tuple[str, str]], dict[tuple[str, str], str]]:
    routes: set[tuple[str, str]] = set()
    auth: dict[tuple[str, str], str] = {}
    for method, path, auth_column in re.findall(
        r"^\| (GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS) \| `([^`]+)` \| ([^|]+) \|",
        docs,
        re.MULTILINE,
    ):
        route = (method, path.split("?", 1)[0])
        routes.add(route)
        auth[route] = auth_column.strip()
    return routes, auth


def openapi_loopback_routes(openapi: dict) -> tuple[set[tuple[str, str]], dict[tuple[str, str], dict]]:
    routes: set[tuple[str, str]] = set()
    operations: dict[tuple[str, str], dict] = {}
    for path, path_item in openapi.get("paths", {}).items():
        for method, operation in path_item.items():
            if not isinstance(operation, dict):
                continue
            if operation.get("x-taarof-surface") != LOOPBACK_HTTP_SURFACE:
                continue
            route = (method.upper(), path)
            routes.add(route)
            operations[route] = operation
    return routes, operations


def route_contract_errors(http_source: str, docs: str, openapi: dict) -> list[str]:
    router = router_routes(http_source)
    documented, documented_auth = documented_routes(docs)
    documented_duplicates = len(
        re.findall(r"^\| (?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS) \|", docs, re.MULTILINE)
    ) != len(documented)
    openapi_routes, openapi_operations = openapi_loopback_routes(openapi)
    errors: list[str] = []
    for name, routes in (("documentation", documented), ("OpenAPI", openapi_routes)):
        missing = sorted(router - routes)
        extra = sorted(routes - router)
        if missing:
            errors.append(f"{name} missing router routes: {missing}")
        if extra:
            errors.append(f"{name} has routes absent from router: {extra}")
    if documented_duplicates:
        errors.append("documentation repeats a local HTTP route")

    for route in sorted(router):
        auth = documented_auth.get(route)
        operation = openapi_operations.get(route)
        if auth is None or operation is None:
            continue
        security = operation.get("security")
        if auth == "No":
            if security != []:
                errors.append(f"{route} is unauthenticated in docs but not OpenAPI")
        elif auth in {"Yes", "Yes + control gate"}:
            expected_security = (
                [{"TaarofLoopbackBearer": []}, {"TaarofLoopbackWebSocketToken": []}]
                if route[1].endswith("/ws") or route[1].endswith("/attach")
                else [{"TaarofLoopbackBearer": []}]
            )
            if security != expected_security:
                errors.append(f"{route} must require TaarofLoopbackBearer in OpenAPI")
        else:
            errors.append(f"{route} has unsupported documentation auth value {auth!r}")
        if auth == "Yes + control gate":
            if operation.get("x-taarof-loopback-control") is not True:
                errors.append(f"{route} must declare the loopback control gate in OpenAPI")
        elif operation.get("x-taarof-loopback-control"):
            errors.append(f"{route} incorrectly declares the loopback control gate")
    return errors


def mutate_translation(socket_source: str, old: str, new: str) -> str:
    """Mutate only the HTTP-to-socket translation arm.

    The async dispatcher repeats these `SocketMessage` patterns earlier in the
    file, so a whole-file `replace` would silently mutate the wrong site and let
    a drift test pass without exercising anything.
    """
    translation = item_body(socket_source, "fn http_control_action_to_socket_message")
    mutated = translation.replace(old, new, 1)
    assert mutated != translation, f"translation mutation did not apply: {old!r}"
    return socket_source.replace(translation, mutated, 1)


def control_action_contract_errors(http_source: str, socket_source: str, protocol_source: str) -> list[str]:
    actions = rust_enum_variants(http_source, "HttpControlAction")
    action_fields = rust_struct_variant_fields(http_source, "HttpControlAction")
    socket_messages = rust_enum_variants(protocol_source, "SocketMessage")
    translation = item_body(socket_source, "fn http_control_action_to_socket_message")
    # KMUX-167 split socket dispatch in two: `dispatch_socket_message` owns the
    # arms that may wait on tmux/SSH, and `handle_socket_message` owns the rest.
    # A control message is dispatched if either layer claims it.
    dispatch = item_body(socket_source, "fn dispatch_socket_message") + item_body(
        socket_source, "fn handle_socket_message"
    )
    handler = item_body(socket_source, "fn handle_http_control_action_async_with_callback")
    errors: list[str] = []

    if "unreachable!(" in translation:
        errors.append("HTTP control translation contains an unreachable exception")
    if "if let crate::http::HttpControlAction::" in handler:
        errors.append("HTTP control handler bypasses SocketMessage translation")

    for action in sorted(actions):
        match = re.search(
            rf"HttpControlAction::{action}\b.*?=>\s*(?:\{{\s*)?(?:SocketMessage::([A-Za-z0-9_]+)|unreachable!)",
            translation,
            re.DOTALL,
        )
        if match is None:
            errors.append(f"{action} has no SocketMessage translation")
            continue
        target = match.group(1)
        if target is None:
            errors.append(f"{action} is an unreachable translation exception")
            continue
        if target != action:
            errors.append(f"{action} translates to SocketMessage::{target}, not itself")
        if target not in socket_messages:
            errors.append(f"{action} translates to undeclared SocketMessage::{target}")
        elif not re.search(rf"\bSocketMessage::{target}\b", dispatch):
            errors.append(f"SocketMessage::{target} is not dispatched by the socket handler")

        field_match = re.search(
            rf"HttpControlAction::{action}\s*\{{(?P<input>.*?)\}}\s*=>\s*"
            rf"(?:\{{\s*)?SocketMessage::{action}\s*\{{(?P<output>.*?)\}}",
            translation,
            re.DOTALL,
        )
        if field_match is None:
            errors.append(f"{action} has no inspectable field-preserving translation")
            continue
        output_fields = named_fields(field_match.group("output"))
        expected_fields = action_fields.get(action, set())
        if set(output_fields) != expected_fields:
            errors.append(
                f"{action} output fields {sorted(output_fields)} do not match {sorted(expected_fields)}"
            )
        for field in sorted(expected_fields):
            if output_fields.get(field) != field:
                errors.append(f"{action}.{field} is not preserved in SocketMessage::{action}")
    return errors


def loopback_semantic_errors(
    http: str, pane_attach: str, pty: str, auth: str, docs: str, openapi: dict
) -> list[str]:
    """Check callable semantics from the Rust handler signatures and bodies."""
    bindings = router_bindings(http)
    _, operations = openapi_loopback_routes(openapi)
    errors: list[str] = []
    if bindings != EXPECTED_ROUTE_HANDLERS:
        errors.append(f"route-to-handler identity drift: {bindings!r}")

    header_security = [{"TaarofLoopbackBearer": []}]
    websocket_security = [
        {"TaarofLoopbackBearer": []},
        {"TaarofLoopbackWebSocketToken": []},
    ]
    if not ("header_matches" in auth and "query_matches" in auth and "header_matches || query_matches" in auth):
        errors.append("check_ws_auth no longer proves header bearer plus query-token alternatives")

    for route, handler in sorted(bindings.items()):
        operation = operations.get(route)
        if operation is None:
            continue
        source = source_for_handler(handler, http, pane_attach, pty)
        header, body = rust_function(source, handler)
        is_websocket = "WebSocketUpgrade" in header
        if operation.get("security") != (websocket_security if is_websocket else ([] if route[1] == "/health" else header_security)):
            errors.append(f"{route} has auth alternatives that do not match its Rust handler")

        responses = operation.get("responses")
        if not isinstance(responses, dict):
            errors.append(f"{route} has no response map")
            continue
        expected_statuses = EXPECTED_RESPONSE_STATUSES.get(route)
        if expected_statuses is None or set(responses) != expected_statuses:
            errors.append(f"{route} response statuses do not match its Rust handler/error path")
        if is_websocket:
            if "101" not in responses or "200" in responses:
                errors.append(f"{route} WebSocket handler must document 101, never 200")
            if "101" in responses:
                upgrade = operation_response(openapi, operation, "101")
                if upgrade.get("content") or set(upgrade.get("headers", {})) != {"Connection", "Upgrade"}:
                    errors.append(f"{route} 101 must be a header-only WebSocket upgrade response")
            required_statuses = {"101", "400", "401"}
            if handler in {"pane_attach_ws", "tab_pane_attach_ws"}:
                required_statuses |= {"404", "409", "429", "500", "503", "504"}
            if handler == "tab_pane_attach_control_ws":
                required_statuses |= {"403", "404", "409", "429", "500", "503", "504"}
            if handler == "tab_pane_pty_ws":
                required_statuses |= {"403", "404", "409", "429", "500", "503", "504"}
            missing = required_statuses - set(responses)
            if missing:
                errors.append(f"{route} omits runtime WebSocket statuses {sorted(missing)}")
        else:
            if "200" not in responses or not schema_has_json_content(openapi, responses["200"]):
                errors.append(f"{route} 200 response lacks an application/json schema")

        for status, raw_response in responses.items():
            response = resolve_ref(openapi, raw_response)
            if not response.get("description"):
                errors.append(f"{route} {status} response has no description")
            for media, media_type in response.get("content", {}).items():
                if not isinstance(media_type, dict) or "schema" not in media_type:
                    errors.append(f"{route} {status} {media} response has no content schema")

        path_parameters = re.findall(r"\{([a-z][A-Za-z0-9_]*)\}", route[1])
        parameters = operation_parameters(openapi, operation)
        for parameter in path_parameters:
            documented = parameters.get(("path", parameter))
            if documented is None or documented.get("required") is not True:
                errors.append(f"{route} path parameter {parameter} is missing or optional")
            elif "integer" not in schema_types(openapi, documented.get("schema", {})):
                errors.append(f"{route} path parameter {parameter} does not match Axum u32 extraction")

        query_match = re.search(r"Query\([^)]*\)\s*:\s*Query<([A-Za-z0-9_]+)>", header)
        if query_match:
            query_fields = rust_struct_fields_any(query_match.group(1), http, pane_attach, pty)
            for name, ty in query_fields.items():
                documented = parameters.get(("query", name))
                if documented is None:
                    errors.append(f"{route} omits Rust query parameter {name}")
                    continue
                kind, optional = rust_type_kind(ty)
                if kind not in schema_types(openapi, documented.get("schema", {})):
                    errors.append(f"{route} query {name} has the wrong schema kind")
                if documented.get("required", False) == optional:
                    errors.append(f"{route} query {name} requiredness does not match {ty}")

        if route[0] != "POST":
            continue
        expected_handler = "control_" + route[1].rsplit("/", 1)[1].replace("-", "_")
        if handler != expected_handler:
            errors.append(f"{route} is routed to {handler}, not {expected_handler}")
            continue
        payload_match = re.search(r"Json\(payload\)\s*:\s*Json<([A-Za-z0-9_]+)>", header)
        action_name = "".join(part.title() for part in route[1].rsplit("/", 1)[1].split("-"))
        if payload_match is None:
            errors.append(f"{route} does not expose an inspectable JSON payload")
            continue
        payload_fields = rust_struct_fields(http, payload_match.group(1))
        action_fields = rust_struct_variant_fields(http, "HttpControlAction").get(action_name, set())
        if set(payload_fields) != action_fields:
            errors.append(f"{route} payload fields do not match HttpControlAction::{action_name}")
        action_match = re.search(rf"HttpControlAction::{action_name}\s*\{{(?P<fields>.*?)\}}", body, re.DOTALL)
        if action_match is None:
            errors.append(f"{route} handler does not build HttpControlAction::{action_name}")
        else:
            assignments = named_fields(action_match.group("fields"))
            for field in action_fields:
                if assignments.get(field) != f"payload.{field}":
                    errors.append(f"{route} does not preserve payload.{field} in its control action")
        request_body = resolve_ref(openapi, operation.get("requestBody", {})) if operation.get("requestBody") else {}
        schema = request_body.get("content", {}).get("application/json", {}).get("schema")
        if not isinstance(schema, dict):
            errors.append(f"{route} lacks an application/json request schema")
            continue
        schema = resolve_ref(openapi, schema)
        properties = schema.get("properties", {})
        required = set(schema.get("required", []))
        if set(properties) != set(payload_fields):
            errors.append(f"{route} request schema fields do not match {payload_match.group(1)}")
        for field, ty in payload_fields.items():
            kind, optional = rust_type_kind(ty)
            if field not in properties or kind not in schema_types(openapi, properties[field]):
                errors.append(f"{route} request schema has wrong type for {field}")
            if optional:
                if field in required or "null" not in schema_types(openapi, properties[field]):
                    errors.append(f"{route} request schema does not model optional nullable {field}")
            elif field not in required:
                errors.append(f"{route} request schema does not require {field}")
        if set(responses) != CONTROL_RESPONSE_STATUSES:
            errors.append(f"{route} control response statuses do not match ControlRouteError and Json extraction")
    if "That header is also the default for WebSocket" not in docs or "WebSocket clients may instead use `?token=<token>`" not in docs:
        errors.append("local-query-api does not state header bearer default plus browser-only WebSocket alternative")
    return errors


def control_runtime_implementation_errors(socket_source: str) -> list[str]:
    # A tmux resize can wait on SSH, so KMUX-167 requires it to be applied by the
    # asynchronous dispatcher; the synchronous GTK handler must refuse the message
    # rather than block the main context.
    errors: list[str] = []
    sync_dispatch = item_body(socket_source, "fn handle_socket_message")
    sync_arm = re.search(
        r"SocketMessage::ResizePane\s*\{[^{}]*\}\s*=>\s*\{(?P<body>[^{}]*)\}", sync_dispatch
    )
    if sync_arm is None or "must use asynchronous socket dispatch" not in sync_arm.group("body"):
        errors.append("SocketMessage::ResizePane is not refused by the synchronous socket handler")
    async_dispatch = item_body(socket_source, "fn dispatch_socket_message")
    resize_dispatch = re.search(
        r"SocketMessage::ResizePane\s*\{.*?\}\s*=>\s*\{.*?\b(dispatch_[A-Za-z0-9_]*)\(",
        async_dispatch,
        re.DOTALL,
    )
    if resize_dispatch is None or resize_dispatch.group(1) != "dispatch_resize_pane_control":
        errors.append("SocketMessage::ResizePane is not routed to dispatch_resize_pane_control")
        return errors
    _, resize = rust_function(socket_source, "dispatch_resize_pane_control")
    required = (
        "PaneControlTarget::Tmux",
        "submit_coalesced",
        "crate::tmux::resize_backing_command",
        "crate::tmux::TMUX_CONTROL_DEADLINE",
        "PaneControlTarget::Vte",
        "terminal.set_size",
    )
    if any(token not in resize for token in required):
        errors.append("dispatch_resize_pane_control no longer performs both tmux and VTE resize operations")
    if re.search(r"SocketResponse::ok\(\)", resize) or len(re.findall(r"SocketResponse::ok_with_data", resize)) != 2:
        errors.append("dispatch_resize_pane_control no longer reports the real resize result")
    return errors


class HttpContractDocsTests(unittest.TestCase):
    maxDiff = None

    def read_openapi(self) -> dict:
        # This repo intentionally keeps this JSON-compatible YAML so the
        # protocol validator and this dependency-free contract can parse it.
        return json.loads(OPENAPI_SOURCE.read_text(encoding="utf-8"))

    def test_router_routes_match_documented_routes(self) -> None:
        self.assertEqual(
            route_contract_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"),
                DOCS_SOURCE.read_text(encoding="utf-8"),
                self.read_openapi(),
            ),
            [],
        )

    def test_router_route_drift_is_detected(self) -> None:
        docs = DOCS_SOURCE.read_text(encoding="utf-8").replace(
            "| GET | `/health` | No |", "| POST | `/health` | No |", 1
        )
        self.assertTrue(
            route_contract_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"), docs, self.read_openapi()
            )
        )

    def test_new_router_method_drift_is_detected(self) -> None:
        router = HTTP_SOURCE.read_text(encoding="utf-8").replace(
            '.route("/health", get(health))',
            '.route("/api/v1/undocumented", delete(health))\n'
            '        .route("/health", get(health))',
            1,
        )
        self.assertTrue(
            route_contract_errors(router, DOCS_SOURCE.read_text(encoding="utf-8"), self.read_openapi())
        )

    def test_chained_router_method_drift_is_detected(self) -> None:
        router = HTTP_SOURCE.read_text(encoding="utf-8").replace(
            '.route("/health", get(health))',
            '.route("/health", get(health).post(health))',
            1,
        )
        self.assertTrue(
            route_contract_errors(router, DOCS_SOURCE.read_text(encoding="utf-8"), self.read_openapi())
        )

    def test_loopback_contract_matches_handler_semantics(self) -> None:
        self.assertEqual(
            loopback_semantic_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"),
                PANE_ATTACH_SOURCE.read_text(encoding="utf-8"),
                PTY_SOURCE.read_text(encoding="utf-8"),
                AUTH_SOURCE.read_text(encoding="utf-8"),
                DOCS_SOURCE.read_text(encoding="utf-8"),
                self.read_openapi(),
            ),
            [],
        )

    def test_loopback_success_responses_are_route_typed(self) -> None:
        self.assertEqual(loopback_response_schema_errors(self.read_openapi()), [])

    def test_generic_success_data_is_detected(self) -> None:
        openapi = self.read_openapi()
        openapi["components"]["schemas"]["LoopbackStateResponse"]["properties"]["data"] = {}
        self.assertTrue(loopback_response_schema_errors(openapi))

    def test_wrong_route_success_schema_is_detected(self) -> None:
        openapi = self.read_openapi()
        openapi["paths"]["/api/v1/workspaces"]["get"]["responses"]["200"] = {
            "$ref": "#/components/responses/LoopbackState"
        }
        self.assertTrue(loopback_response_schema_errors(openapi))

    def test_canonical_http_docs_do_not_claim_the_surface_is_read_only(self) -> None:
        self.assertEqual(canonical_http_claim_errors(DOCS_SOURCE.read_text(encoding="utf-8")), [])

    def test_false_canonical_read_only_claim_is_detected(self) -> None:
        docs = DOCS_SOURCE.read_text(encoding="utf-8").replace(
            "existing loopback HTTP\nsurface, which is read-only by default but has a separately gated local control\nsubset",
            "existing read-only HTTP surface",
            1,
        )
        self.assertTrue(canonical_http_claim_errors(docs))

    def test_split_route_wrong_handler_is_detected(self) -> None:
        http = HTTP_SOURCE.read_text(encoding="utf-8").replace(
            'post(control_split_pane)', 'post(control_create_tab)', 1
        )
        self.assertTrue(
            loopback_semantic_errors(
                http,
                PANE_ATTACH_SOURCE.read_text(encoding="utf-8"),
                PTY_SOURCE.read_text(encoding="utf-8"),
                AUTH_SOURCE.read_text(encoding="utf-8"),
                DOCS_SOURCE.read_text(encoding="utf-8"),
                self.read_openapi(),
            )
        )

    def test_websocket_599_is_detected(self) -> None:
        openapi = self.read_openapi()
        openapi["paths"]["/api/v1/events/ws"]["get"]["responses"]["599"] = openapi["paths"]["/api/v1/events/ws"]["get"]["responses"].pop("101")
        self.assertTrue(
            loopback_semantic_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"),
                PANE_ATTACH_SOURCE.read_text(encoding="utf-8"),
                PTY_SOURCE.read_text(encoding="utf-8"),
                AUTH_SOURCE.read_text(encoding="utf-8"),
                DOCS_SOURCE.read_text(encoding="utf-8"),
                openapi,
            )
        )

    def test_absent_control_schema_is_detected(self) -> None:
        openapi = self.read_openapi()
        del openapi["paths"]["/api/v1/control/send-keys"]["post"]["requestBody"]
        self.assertTrue(
            loopback_semantic_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"),
                PANE_ATTACH_SOURCE.read_text(encoding="utf-8"),
                PTY_SOURCE.read_text(encoding="utf-8"),
                AUTH_SOURCE.read_text(encoding="utf-8"),
                DOCS_SOURCE.read_text(encoding="utf-8"),
                openapi,
            )
        )

    def test_http_control_actions_match_socket_messages(self) -> None:
        self.assertEqual(
            control_action_contract_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"),
                SOCKET_SOURCE.read_text(encoding="utf-8"),
                PROTOCOL_SOURCE.read_text(encoding="utf-8"),
            ),
            [],
        )

    def test_http_control_translation_drift_is_detected(self) -> None:
        socket = mutate_translation(
            SOCKET_SOURCE.read_text(encoding="utf-8"),
            "SocketMessage::SendKeys { tab, pane, keys }",
            "SocketMessage::UntranslatedSendKeys { tab, pane, keys }",
        )
        self.assertTrue(
            control_action_contract_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"),
                socket,
                PROTOCOL_SOURCE.read_text(encoding="utf-8"),
            )
        )

    def test_http_control_payload_drift_is_detected(self) -> None:
        socket = mutate_translation(
            SOCKET_SOURCE.read_text(encoding="utf-8"),
            """SocketMessage::ResizePane {
            tab,
            pane,
            cols,
            rows,
        }""",
            """SocketMessage::ResizePane {
            tab,
            pane,
            cols: rows,
            rows: cols,
        }""",
        )
        self.assertTrue(
            control_action_contract_errors(
                HTTP_SOURCE.read_text(encoding="utf-8"),
                socket,
                PROTOCOL_SOURCE.read_text(encoding="utf-8"),
            )
        )

    def test_resize_handler_socket_response_ok_is_detected(self) -> None:
        original = SOCKET_SOURCE.read_text(encoding="utf-8")
        socket = original.replace(
            'SocketResponse::ok_with_data(serde_json::json!({\n'
            '                                    "supported": true,\n'
            '                                    "backend": "tmux",',
            'SocketResponse::ok()',
            1,
        )
        self.assertNotEqual(socket, original)
        self.assertTrue(control_runtime_implementation_errors(socket))

    def test_resize_handler_runs_real_backends(self) -> None:
        self.assertEqual(control_runtime_implementation_errors(SOCKET_SOURCE.read_text(encoding="utf-8")), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
