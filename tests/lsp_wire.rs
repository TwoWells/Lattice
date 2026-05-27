// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Wire-level integration tests for the LSP server.

#![allow(
    clippy::expect_used,
    reason = "integration tests use expect for clarity"
)]
//!
//! Spawns `lattice serve` as a subprocess and communicates via LSP messages
//! on stdin/stdout to verify end-to-end request/response handling.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// Format a JSON body as an LSP wire message (`Content-Length` header + body).
fn lsp_message(body: &serde_json::Value) -> Vec<u8> {
    let json = serde_json::to_string(body).expect("serialize JSON");
    format!("Content-Length: {}\r\n\r\n{}", json.len(), json).into_bytes()
}

/// Read one LSP message from a buffered reader, returning the parsed JSON body.
fn read_message(reader: &mut BufReader<impl Read>) -> serde_json::Value {
    // Read headers until blank line.
    let mut content_length: usize = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("read header line");
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some(len) = header.strip_prefix("Content-Length: ") {
            content_length = len.parse().expect("parse Content-Length");
        }
    }
    assert!(
        content_length > 0,
        "Content-Length must be present and non-zero"
    );

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).expect("read message body");
    serde_json::from_slice(&body).expect("parse JSON body")
}

/// Spawn a `lattice serve` subprocess, returning (stdin writer, stdout reader, child).
fn spawn_server() -> (impl Write, BufReader<impl Read>, std::process::Child) {
    let bin = env!("CARGO_BIN_EXE_lattice");
    let mut child = Command::new(bin)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lattice serve");

    let stdin = child.stdin.take().expect("capture stdin");
    let stdout = BufReader::new(child.stdout.take().expect("capture stdout"));

    (stdin, stdout, child)
}

/// Send an initialize request and read the response, returning the initialized
/// server (stdin, stdout, child). Sends `initialized` notification afterward.
fn initialize_server(root_uri: &str, stdin: &mut impl Write, stdout: &mut BufReader<impl Read>) {
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "processId": null,
            "capabilities": {},
            "workspaceFolders": [
                { "uri": root_uri, "name": "test" }
            ]
        }
    });
    stdin
        .write_all(&lsp_message(&init_req))
        .expect("send initialize");
    stdin.flush().expect("flush");

    let resp = read_message(stdout);
    assert_eq!(resp["id"], 0, "initialize response should have id 0");
    assert!(resp.get("result").is_some(), "initialize should succeed");

    // Send initialized notification.
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    stdin
        .write_all(&lsp_message(&initialized))
        .expect("send initialized");
    stdin.flush().expect("flush");
}

/// Send a shutdown request and exit notification to cleanly stop the server.
///
/// Takes ownership of `stdin` so it can be dropped (closing the pipe) before
/// waiting for the child process to exit.
fn shutdown_server(
    mut stdin: impl Write,
    stdout: &mut BufReader<impl Read>,
    mut child: std::process::Child,
) {
    let shutdown = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 9999,
        "method": "shutdown",
        "params": null
    });
    stdin
        .write_all(&lsp_message(&shutdown))
        .expect("send shutdown");
    stdin.flush().expect("flush");

    let resp = read_message(stdout);
    assert_eq!(resp["id"], 9999, "shutdown response id should match");

    let exit = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "exit",
        "params": null
    });
    stdin.write_all(&lsp_message(&exit)).expect("send exit");
    stdin.flush().expect("flush");
    drop(stdin); // Close the pipe so the server's reader thread exits.

    let status = child.wait().expect("wait for child");
    assert!(status.success(), "server should exit cleanly");
}

/// Helper: create a temp workspace with files, returning the temp dir.
fn workspace_with_files(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    std::fs::create_dir(dir.path().join(".git")).expect("create .git");
    for (path, content) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&full, content).expect("write file");
    }
    dir
}

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Skip any publishDiagnostics notifications, returning the next non-notification message.
fn read_next_response(stdout: &mut BufReader<impl Read>) -> serde_json::Value {
    loop {
        let msg = read_message(stdout);
        // Notifications have "method" but no "id".
        if msg.get("method").is_some() && msg.get("id").is_none() {
            continue;
        }
        return msg;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn workspace_symbol_returns_headings() {
    let dir = workspace_with_files(&[("a.md", "# Alpha\n\n## Beta\n"), ("b.md", "# Gamma\n")]);
    let root_uri = path_to_uri(dir.path());

    let (mut stdin, mut stdout, child) = spawn_server();
    initialize_server(&root_uri, &mut stdin, &mut stdout);

    // Send workspace/symbol request.
    let symbol_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "workspace/symbol",
        "params": { "query": "" }
    });
    stdin
        .write_all(&lsp_message(&symbol_req))
        .expect("send workspace/symbol");
    stdin.flush().expect("flush");

    let resp = read_next_response(&mut stdout);
    assert_eq!(resp["id"], 1, "response id should match request");
    assert!(
        resp.get("error").is_none(),
        "workspace/symbol should not return an error: {resp}"
    );

    let result = resp["result"]
        .as_array()
        .expect("result should be an array");
    let names: Vec<&str> = result.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"Alpha"), "should contain Alpha: {names:?}");
    assert!(names.contains(&"Beta"), "should contain Beta: {names:?}");
    assert!(names.contains(&"Gamma"), "should contain Gamma: {names:?}");

    shutdown_server(stdin, &mut stdout, child);
}

#[test]
fn malformed_request_returns_error_response() {
    let dir = workspace_with_files(&[("a.md", "# A\n")]);
    let root_uri = path_to_uri(dir.path());

    let (mut stdin, mut stdout, child) = spawn_server();
    initialize_server(&root_uri, &mut stdin, &mut stdout);

    // Send a workspace/symbol request with invalid params (missing "query" field).
    let bad_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "workspace/symbol",
        "params": { "not_a_real_field": 42 }
    });
    stdin
        .write_all(&lsp_message(&bad_req))
        .expect("send bad request");
    stdin.flush().expect("flush");

    let resp = read_next_response(&mut stdout);
    assert_eq!(resp["id"], 1, "error response id should match request");
    assert!(
        resp.get("error").is_some(),
        "malformed request should return an error response: {resp}"
    );

    // Server should still be alive — send another valid request.
    let symbol_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "workspace/symbol",
        "params": { "query": "" }
    });
    stdin
        .write_all(&lsp_message(&symbol_req))
        .expect("send valid request after error");
    stdin.flush().expect("flush");

    let resp = read_next_response(&mut stdout);
    assert_eq!(resp["id"], 2, "second response id should match");
    assert!(
        resp.get("error").is_none(),
        "server should still work after handling a bad request: {resp}"
    );

    shutdown_server(stdin, &mut stdout, child);
}
