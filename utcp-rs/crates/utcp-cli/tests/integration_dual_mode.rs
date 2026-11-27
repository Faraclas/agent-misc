use axum::{
    extract::{Path as AxumPath, Query},
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::TempDir;

/// The `utcp` binary under test. `UTCP_TEST_BIN` overrides the build-tree path, so the compiled
/// test binary can run on a machine without the source tree (a Windows VM).
fn utcp_bin() -> String {
    std::env::var("UTCP_TEST_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_utcp").to_string())
}

struct IsolatedEnv {
    tmp: TempDir,
}

impl IsolatedEnv {
    fn new() -> Self {
        Self {
            tmp: TempDir::new().expect("tempdir"),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(utcp_bin());
        let root = self.tmp.path();
        c.env("UTCP_CONFIG_DIR", root.join("config"))
            .env("UTCP_CACHE_DIR", root.join("cache"))
            .env("UTCP_STATE_DIR", root.join("state"))
            .env("UTCP_MEDIA_DIR", root.join("media"))
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"));
        #[cfg(unix)]
        c.env("UTCP_SOCKET_PATH", root.join("utcp-test.sock"));
        // One pipe per test: tests in this binary run in parallel and share a process id.
        #[cfg(windows)]
        c.env(
            "UTCP_SOCKET_PATH",
            format!(
                r"\\.\pipe\utcp-test-{}",
                root.file_name().unwrap().to_string_lossy()
            ),
        );
        c
    }
}

// A test that fails midway must not leave its daemon (and the servers it started) running.
impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        let _ = self.cmd().args(["daemon", "stop"]).output();
    }
}

async fn spawn_mock_api() -> String {
    let openapi_handler = |headers: HeaderMap| async move {
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("127.0.0.1");
        Json(json!({
            "openapi": "3.0.3",
            "info": {
                "title": "Local Mock Service",
                "version": "1.0.0"
            },
            "servers": [
                { "url": format!("http://{}", host) }
            ],
            "components": {
                "securitySchemes": {
                    "bearerAuth": {
                        "type": "http",
                        "scheme": "bearer"
                    }
                }
            },
            "security": [{ "bearerAuth": [] }],
            "paths": {
                "/users/{user_id}": {
                    "get": {
                        "operationId": "get_user",
                        "summary": "Fetch user details by ID",
                        "tags": ["users"],
                        "parameters": [
                            { "name": "user_id", "in": "path", "required": true, "schema": { "type": "string" } },
                            { "name": "verbose", "in": "query", "required": false, "schema": { "type": "boolean" } }
                        ],
                        "responses": {
                            "200": {
                                "description": "User record",
                                "content": {
                                    "application/json": {
                                        "schema": {
                                            "type": "object",
                                            "properties": { "id": { "type": "string" } }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "/transcribe": {
                    "post": {
                        "operationId": "transcribe_audio",
                        "summary": "Transcribe voice snippet",
                        "tags": ["voice"],
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": { "text": { "type": "string" } }
                                    }
                                }
                            }
                        },
                        "responses": {
                            "200": { "description": "Transcription output" }
                        }
                    }
                }
            }
        }))
    };

    let get_user = |AxumPath(user_id): AxumPath<String>,
                    Query(q): Query<HashMap<String, String>>,
                    headers: HeaderMap| async move {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        Json(json!({
            "user_id": user_id,
            "verbose": q.get("verbose").cloned(),
            "auth": auth
        }))
    };

    let transcribe = |Json(body): Json<Value>| async move {
        Json(json!({
            "transcribed": body
        }))
    };

    let app = Router::new()
        .route("/openapi.json", get(openapi_handler))
        .route("/users/:user_id", get(get_user))
        .route("/transcribe", post(transcribe));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_dual_mode_lazy_disclosure_and_concurrency() {
    let env = Arc::new(IsolatedEnv::new());
    let base_url = spawn_mock_api().await;
    let spec_url = format!("{}/openapi.json", base_url);

    // 1. Mode B: `utcp import` without registering
    let standalone_out = env.tmp.path().join("standalone.utcp.json");
    let out = env
        .cmd()
        .args([
            "import",
            &spec_url,
            "-o",
            standalone_out.to_str().unwrap(),
            "--name",
            "voice_svc",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "import failed: {:?}", out);
    assert!(standalone_out.exists());
    assert!(
        !env.tmp.path().join("config/.utcp_config.json").exists(),
        "import must not modify .utcp_config.json"
    );

    // 2. Mode A: `utcp register` (cache_on_register = true by default)
    let out = env
        .cmd()
        .args(["register", "my_api", &spec_url])
        .output()
        .unwrap();
    assert!(out.status.success(), "register failed: {:?}", out);
    assert!(env
        .tmp
        .path()
        .join("cache/manuals/my_api.utcp.json")
        .exists());
    assert!(env.tmp.path().join("cache/search_index.bin").exists());

    // Write doubled-underscore `.env` credential (`my__api_API_KEY`)
    std::fs::write(
        env.tmp.path().join("config/.env"),
        "my__api_API_KEY=super-secret-token\n",
    )
    .unwrap();

    // 3. Lazy progressive disclosure in Direct Mode:
    //    `search` returns compact summary (no schema bloat), `schema` returns full JSON schema
    let search_out = env.cmd().args(["search", "user"]).output().unwrap();
    assert!(search_out.status.success());
    let search_stdout = String::from_utf8_lossy(&search_out.stdout);
    assert!(search_stdout.contains("my_api.get_user"));
    assert!(!search_stdout.contains("\"properties\""));

    let schema_out = env
        .cmd()
        .args(["schema", "my_api.get_user"])
        .output()
        .unwrap();
    assert!(schema_out.status.success());
    let schema_json: Value = serde_json::from_slice(&schema_out.stdout).unwrap();
    assert_eq!(schema_json["name"], "my_api.get_user");
    assert!(schema_json["inputs"]["properties"]["user_id"].is_object());

    // 4. `--dry-run` redacts secret token
    let dry_out = env
        .cmd()
        .args([
            "call",
            "my_api.get_user",
            r#"{"user_id": "alice", "verbose": true}"#,
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(dry_out.status.success());
    let dry_str = String::from_utf8_lossy(&dry_out.stdout);
    assert!(!dry_str.contains("super-secret-token"));
    assert!(dry_str.contains("REDACTED"));

    // 5. Direct Mode `call` via `--args-stdin` (cross-platform PowerShell/POSIX safe)
    let mut child = env
        .cmd()
        .args(["call", "my_api.get_user", "--args-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"user_id": "alice", "verbose": true}"#)
        .unwrap();
    let call_out = child.wait_with_output().unwrap();
    assert!(call_out.status.success(), "call failed: {:?}", call_out);
    let call_val: Value = serde_json::from_slice(&call_out.stdout).unwrap();
    assert_eq!(call_val["user_id"], "alice");
    assert_eq!(call_val["verbose"], "true");
    assert_eq!(call_val["auth"], "Bearer super-secret-token");

    // Verify opt-in activity log is STILL absent by default
    assert!(!env.tmp.path().join("state/activity.jsonl").exists());

    // 6. Start `utcp daemon start` and hammer it with 20 concurrent callers!
    let d_start = env.cmd().args(["daemon", "start"]).output().unwrap();
    assert!(
        d_start.status.success(),
        "daemon start failed: {:?}",
        d_start
    );

    let mut handles = Vec::new();
    for i in 0..20 {
        let env_cl = env.clone();
        handles.push(std::thread::spawn(move || {
            let arg_str = format!(r#"{{"user_id": "agent_{}"}}"#, i);
            let out = env_cl
                .cmd()
                .args(["call", "my_api.get_user", &arg_str])
                .output()
                .unwrap();
            assert!(out.status.success());
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(
                v["user_id"],
                format!("agent_{}", i),
                "daemon response routing must never mix concurrent callers!"
            );
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // 7. Batch execution over Daemon Mode
    let mut batch_child = env
        .cmd()
        .args(["batch", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    batch_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            br#"[
                {"id": "a", "tool": "my_api.get_user", "args": {"user_id": "one"}},
                {"id": "b", "tool": "my_api.transcribe_audio", "args": {"body": {"text": "hello world"}}}
            ]"#,
        )
        .unwrap();
    let batch_out = batch_child.wait_with_output().unwrap();
    assert!(batch_out.status.success());
    let batch_val: Value = serde_json::from_slice(&batch_out.stdout).unwrap();
    assert_eq!(batch_val[0]["result"]["user_id"], "one");
    assert_eq!(batch_val[1]["result"]["transcribed"]["text"], "hello world");

    // 8. Stop daemon (`utcp daemon stop`) and confirm seamless fallback to Direct Mode!
    let d_stop = env.cmd().args(["daemon", "stop"]).output().unwrap();
    assert!(d_stop.status.success());

    let fallback_out = env
        .cmd()
        .args(["call", "my_api.get_user", r#"{"user_id": "after_stop"}"#])
        .output()
        .unwrap();
    assert!(fallback_out.status.success());
    let fb_val: Value = serde_json::from_slice(&fallback_out.stdout).unwrap();
    assert_eq!(fb_val["user_id"], "after_stop");

    // 9. Opt-in logging (`utcp log on`) + IDE skill sync (`utcp sync-ides`)
    assert!(env
        .cmd()
        .args(["log", "on"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(env
        .cmd()
        .args(["search", "transcribe"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(env.tmp.path().join("state/activity.jsonl").exists());

    let ws_dir = env.tmp.path().join("workspace");
    std::fs::create_dir_all(&ws_dir).unwrap();
    let sync_out = env
        .cmd()
        .args([
            "sync-ides",
            "--global",
            "--workspace",
            ws_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(sync_out.status.success());
    assert!(ws_dir.join(".agents/skills/utcp-tools/SKILL.md").exists());
    assert!(ws_dir.join(".agents/skills/utcp-admin/SKILL.md").exists());
    assert!(ws_dir
        .join(".agents/skills/utcp-admin/operating.md")
        .exists());
    let home_skills = env.tmp.path().join("home/.agents/skills");
    assert!(home_skills.join("utcp-tools/SKILL.md").exists());
    assert!(home_skills.join("utcp-admin/SKILL.md").exists());
    assert!(home_skills.join("utcp-admin/operating.md").exists());

    // 10. SSRF metadata block check (`169.254.169.254` must be rejected immediately)
    let ssrf_out = env
        .cmd()
        .args([
            "register",
            "bad_meta",
            "http://169.254.169.254/latest/meta-data/",
        ])
        .output()
        .unwrap();
    assert!(!ssrf_out.status.success());
    let stderr = String::from_utf8_lossy(&ssrf_out.stderr);
    assert!(stderr.contains("SSRF protection blocked"));
}

fn run_ok(env: &IsolatedEnv, args: &[&str]) -> String {
    let out = env.cmd().args(args).output().unwrap();
    assert!(
        out.status.success(),
        "utcp {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `utcp ... | head` closes stdout before utcp is done: it finishes quietly instead of panicking.
#[test]
fn closed_stdout_ends_the_output_without_a_panic() {
    let env = IsolatedEnv::new();
    let mut child = env
        .cmd()
        .args(["sync-ides", "--print-skill"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(out.status.success(), "{stderr}");
}

fn run_err(env: &IsolatedEnv, args: &[&str]) -> String {
    let out = env.cmd().args(args).output().unwrap();
    assert!(!out.status.success(), "utcp {:?} should have failed", args);
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Waits (the daemon writes the disk log on a background task) until `activity.jsonl` holds an
/// entry matching `pred`, then returns it.
fn wait_for_logged_activity(env: &IsolatedEnv, pred: impl Fn(&Value) -> bool) -> Value {
    let log = env.tmp.path().join("state/activity.jsonl");
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&log) {
            if let Some(v) = text
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| pred(v))
            {
                return v;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!(
        "no matching entry in activity.jsonl:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_calls_keep_redacted_error_detail_in_activity() {
    let env = IsolatedEnv::new();
    let base_url = spawn_mock_api().await;
    let spec_url = format!("{}/openapi.json", base_url);
    run_ok(&env, &["register", "my_api", &spec_url]);
    std::fs::write(
        env.tmp.path().join("config/.env"),
        "my__api_API_KEY=super-secret-token\n",
    )
    .unwrap();
    run_ok(&env, &["log", "on"]);
    run_ok(&env, &["daemon", "start"]);

    // A failing call through the daemon. The bad tool name echoes the secret value, so the
    // recorded detail must show the variable name instead.
    let stderr = run_err(&env, &["call", "my_api.super-secret-token", "{}"]);
    assert!(stderr.contains("super-secret-token"), "{stderr}");
    let entry = wait_for_logged_activity(&env, |v| {
        v["command"] == "call" && v["ok"] == false && v["mode"] == "daemon"
    });
    assert_eq!(entry["status"], "ERR");
    let detail = entry["detail"].as_str().expect("failed call keeps detail");
    assert!(!detail.contains("super-secret-token"), "{detail}");
    assert!(
        detail.contains("***REDACTED:my__api_API_KEY***"),
        "{detail}"
    );

    // Successful calls stay small: no `detail` key at all.
    run_ok(&env, &["call", "my_api.get_user", r#"{"user_id": "ok"}"#]);
    let ok_entry = wait_for_logged_activity(&env, |v| v["command"] == "call" && v["ok"] == true);
    assert!(ok_entry.get("detail").is_none(), "{ok_entry}");

    // A half-failed batch reports the count and names the failed item.
    let mut batch = env
        .cmd()
        .args(["batch", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    batch
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            br#"[
                {"id": "a", "tool": "my_api.get_user", "args": {"user_id": "one"}},
                {"id": "b", "tool": "my_api.no_such_tool", "args": {}}
            ]"#,
        )
        .unwrap();
    let _ = batch.wait_with_output().unwrap();
    let batch_entry = wait_for_logged_activity(&env, |v| v["command"] == "batch");
    assert_eq!(batch_entry["ok"], false);
    assert_eq!(batch_entry["status"], "1/2 OK");
    let batch_detail = batch_entry["detail"].as_str().unwrap();
    assert!(
        batch_detail.starts_with("b my_api.no_such_tool:"),
        "{batch_detail}"
    );

    // With disk logging off, `log recent` still shows failures from the daemon's memory.
    run_ok(&env, &["log", "off"]);
    let log_size = std::fs::metadata(env.tmp.path().join("state/activity.jsonl"))
        .map(|m| m.len())
        .ok();
    run_err(&env, &["call", "my_api.memory_only_missing", "{}"]);
    let recent: Value =
        serde_json::from_str(&run_ok(&env, &["log", "recent", "--failed", "--json"])).unwrap();
    let newest = &recent.as_array().unwrap()[0];
    assert_eq!(newest["ok"], false);
    assert!(
        newest["detail"]
            .as_str()
            .unwrap()
            .contains("memory_only_missing"),
        "{newest}"
    );
    assert!(recent.as_array().unwrap().iter().all(|e| e["ok"] == false));
    let text = run_ok(&env, &["log", "recent", "--limit", "1"]);
    assert!(text.contains("FAIL"), "{text}");
    assert!(
        text.contains("    "),
        "detail is indented below the call: {text}"
    );
    assert!(text.contains("memory_only_missing"), "{text}");
    assert_eq!(
        std::fs::metadata(env.tmp.path().join("state/activity.jsonl"))
            .map(|m| m.len())
            .ok(),
        log_size,
        "nothing written to disk while logging is off"
    );
    run_ok(&env, &["log", "on"]);

    // Direct mode (no daemon) records the detail too.
    run_ok(&env, &["daemon", "stop"]);
    run_err(&env, &["call", "my_api.direct_missing", "{}"]);
    let direct = wait_for_logged_activity(&env, |v| {
        v["command"] == "call" && v["ok"] == false && v["mode"] != "daemon"
    });
    assert!(
        direct["detail"]
            .as_str()
            .unwrap()
            .contains("direct_missing"),
        "{direct}"
    );
}

/// Stands in for the daemon on the test socket: answers `replies.len()` requests in order and
/// returns each request the CLI sent, so a test can see exactly what crossed the socket.
#[cfg(unix)]
fn capture_ipc_requests(
    env: &IsolatedEnv,
    replies: Vec<Value>,
) -> std::thread::JoinHandle<Vec<Value>> {
    use std::io::Read;
    let listener =
        std::os::unix::net::UnixListener::bind(env.tmp.path().join("utcp-test.sock")).unwrap();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).unwrap();
            let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
            stream.read_exact(&mut buf).unwrap();
            seen.push(serde_json::from_slice::<Value>(&buf).unwrap());
            let out = serde_json::to_vec(&reply).unwrap();
            stream.write_all(&(out.len() as u32).to_le_bytes()).unwrap();
            stream.write_all(&out).unwrap();
        }
        seen
    })
}

// The CLI sends the daemon only the shell variables a registered manual can read: never the rest
// of the caller's environment, for `call`, `batch` and `daemon reload-env` alike.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_sends_daemon_only_manual_env_vars() {
    let env = IsolatedEnv::new();
    let base_url = spawn_mock_api().await;
    run_ok(
        &env,
        &["register", "my_api", &format!("{}/openapi.json", base_url)],
    );

    let server = capture_ipc_requests(
        &env,
        vec![
            json!({"status": "ok", "data": {"kind": "call_result", "value": {}}}),
            json!({"status": "ok", "data": {"kind": "batch_results", "value": []}}),
            json!({"status": "ok", "data": {"kind": "env_reloaded", "value": {"total_vars": 1}}}),
        ],
    );
    let shell = |c: &mut Command| {
        c.env("my__api_API_KEY", "needed-secret")
            .env("UNRELATED_SECRET_TOKEN", "must-not-be-sent");
    };

    let mut call = env.cmd();
    shell(&mut call);
    let out = call
        .args(["call", "my_api.get_user", r#"{"user_id": "x"}"#])
        .output()
        .unwrap();
    assert!(out.status.success(), "call failed: {:?}", out);

    let mut batch = env.cmd();
    shell(&mut batch);
    let mut child = batch
        .args(["batch", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"[{"id": "a", "tool": "my_api.get_user", "args": {"user_id": "y"}}]"#)
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());

    let mut reload = env.cmd();
    shell(&mut reload);
    let out = reload.args(["daemon", "reload-env"]).output().unwrap();
    assert!(out.status.success(), "reload-env failed: {:?}", out);

    let seen = server.join().unwrap();
    let kinds: Vec<_> = seen.iter().map(|r| r["command"]["type"].clone()).collect();
    assert_eq!(kinds, ["call", "batch", "reload_env"]);
    for req in &seen {
        assert_eq!(
            req["command"]["caller_env"],
            json!({"my__api_API_KEY": "needed-secret"}),
            "CLI sent more than the manual's variables"
        );
    }
}

fn read_config(env: &IsolatedEnv) -> Value {
    let raw = std::fs::read_to_string(env.tmp.path().join("config/.utcp_config.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

/// Deregister keeps the entry and saved copy; `register <name>` (no source) restores it offline;
/// `--purge-cache` deletes both. Also covers a saved file in `manuals/` with no registry entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deregister_reregister_and_purge_round_trip() {
    let env = IsolatedEnv::new();
    let base_url = spawn_mock_api().await;
    let spec_url = format!("{}/openapi.json", base_url);
    let saved = env.tmp.path().join("cache/manuals/pets.utcp.json");

    run_ok(&env, &["register", "pets", &spec_url, "-H", "X-Team:blue"]);
    assert!(saved.exists());
    assert!(run_ok(&env, &["list", "--tools"]).contains("pets."));

    // 1. Deregister: gone from list/search, but entry and saved copy are kept.
    let out = run_ok(&env, &["deregister", "pets"]);
    assert!(
        out.contains("utcp register pets"),
        "restore hint missing: {out}"
    );
    assert!(!run_ok(&env, &["list", "--tools"]).contains("pets."));
    assert!(!run_ok(&env, &["search", "user"]).contains("pets."));
    let cfg = read_config(&env);
    assert!(cfg["manuals"].get("pets").is_none());
    assert_eq!(cfg["deregistered"]["pets"]["source_url"], spec_url.as_str());
    assert!(saved.exists(), "plain deregister keeps the saved copy");

    // Deregistering again finds nothing active.
    assert!(run_ok(&env, &["deregister", "pets"]).contains("was not registered"));
    // Re-register flags need a source.
    assert!(
        run_err(&env, &["register", "pets", "--base-url", "http://x"]).contains("need a source")
    );

    // 2. Re-register without a source: offline, settings (source, headers) restored.
    let out = run_ok(&env, &["register", "pets"]);
    assert!(
        out.contains("Re-registered 'pets' from its saved copy"),
        "{out}"
    );
    assert!(run_ok(&env, &["list", "--tools"]).contains("pets."));
    let cfg = read_config(&env);
    assert_eq!(cfg["manuals"]["pets"]["source_url"], spec_url.as_str());
    assert_eq!(cfg["manuals"]["pets"]["headers"]["X-Team"], "blue");
    assert!(
        cfg.get("deregistered").is_none(),
        "empty deregistered map is not written"
    );
    assert!(run_err(&env, &["register", "pets"]).contains("already registered"));

    // 3. Deregister then purge an already-deregistered manual: entry and file are both gone.
    run_ok(&env, &["deregister", "pets"]);
    assert!(run_ok(&env, &["deregister", "pets", "--purge-cache"]).contains("Purged manual 'pets'"));
    assert!(!saved.exists());
    let cfg = read_config(&env);
    assert!(cfg["manuals"].get("pets").is_none());
    assert!(cfg.get("deregistered").is_none());
    let err = run_err(&env, &["register", "pets"]);
    assert!(err.contains("No deregistered manual named 'pets'"), "{err}");

    // 4. A saved file with no registry entry (orphan) can be registered by name and purged.
    let stray = env.tmp.path().join("cache/manuals/stray.utcp.json");
    run_ok(
        &env,
        &[
            "import",
            &spec_url,
            "--name",
            "stray",
            "-o",
            stray.to_str().unwrap(),
        ],
    );
    assert!(!run_ok(&env, &["list", "--tools"]).contains("stray."));
    let err = run_err(&env, &["register", "nope"]);
    assert!(err.contains("available: stray"), "{err}");
    run_ok(&env, &["register", "stray"]);
    assert!(run_ok(&env, &["list", "--tools"]).contains("stray."));
    run_ok(&env, &["deregister", "stray", "--purge-cache"]);
    assert!(!stray.exists());
    assert!(!run_ok(&env, &["list", "--tools"]).contains("stray."));
}

/// The `description` line of the installed Zed skill (`~/.agents/skills`) in the isolated HOME.
fn installed_skill_description(env: &IsolatedEnv) -> String {
    let skill = std::fs::read_to_string(
        env.tmp
            .path()
            .join("home/.agents/skills/utcp-tools/SKILL.md"),
    )
    .unwrap();
    let front = skill.split("\n---\n").next().unwrap().to_string();
    // The folded YAML description spans several lines; join them like YAML does.
    front.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Hints: `--hint` on register, `set-hint` / `get-hint`, kept across re-registration and
/// deregistration, and shown next to the manual name in the installed skill's description.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_hints_flow_into_the_installed_skill_description() {
    let env = IsolatedEnv::new();
    let base_url = spawn_mock_api().await;
    let spec_url = format!("{}/openapi.json", base_url);
    run_ok(&env, &["sync-ides", "--global"]);

    // 1. register --hint (whitespace normalized) and register without one.
    let out = run_ok(
        &env,
        &[
            "register",
            "pets",
            &spec_url,
            "--hint",
            "  pet   store demo ",
        ],
    );
    assert!(out.contains("Hint      : pet store demo"), "{out}");
    let out = run_ok(&env, &["register", "other", &spec_url]);
    assert!(
        out.contains("Hint      : none; agents see only 'other'")
            && out.contains("utcp set-hint other"),
        "{out}"
    );
    assert!(
        installed_skill_description(&env)
            .contains("(registered manuals: other, pets (pet store demo))"),
        "{}",
        installed_skill_description(&env)
    );

    // 2. get-hint: one manual, or all of them.
    assert_eq!(run_ok(&env, &["get-hint", "pets"]).trim(), "pet store demo");
    assert!(run_ok(&env, &["get-hint", "other"]).contains("No hint set for 'other'"));
    let all = run_ok(&env, &["get-hint"]);
    assert!(
        all.contains("other  -") && all.contains("pets   pet store demo"),
        "{all}"
    );
    assert!(run_err(&env, &["get-hint", "nope"]).contains("No manual named 'nope'"));

    // 3. set-hint updates the skill; --clear removes it; bad input changes nothing.
    let out = run_ok(&env, &["set-hint", "other", "second api"]);
    assert!(out.contains("other (second api)"), "{out}");
    assert!(installed_skill_description(&env).contains("other (second api), pets (pet store demo)"));
    // Hint words are searchable, so `utcp search second` finds the manual's tools.
    let hits = run_ok(&env, &["search", "second", "--json"]);
    assert!(
        hits.contains("other.get_user") && !hits.contains("pets."),
        "{hits}"
    );
    run_ok(&env, &["set-hint", "other", "--clear"]);
    assert!(!run_ok(&env, &["search", "second", "--json"]).contains("other."));
    assert!(installed_skill_description(&env).contains("manuals: other, pets (pet store demo))"));
    assert!(read_config(&env)["manuals"]["other"].get("hint").is_none());
    let long = "x".repeat(61);
    assert!(run_err(&env, &["set-hint", "other", &long]).contains("a few words"));
    assert!(run_err(&env, &["set-hint", "nope", "x"]).contains("No manual named 'nope'"));
    run_err(&env, &["set-hint", "other"]);
    run_err(&env, &["set-hint", "other", "x", "--clear"]);
    assert!(run_err(&env, &["import", &spec_url, "--hint", "x"]).contains("--register"));

    // 4. Re-registering from the source without --hint keeps the hint.
    run_ok(&env, &["register", "pets", &spec_url]);
    assert_eq!(
        read_config(&env)["manuals"]["pets"]["hint"],
        "pet store demo"
    );

    // 5. Deregistered: dropped from the skill, hint kept and editable; re-register --hint wins.
    run_ok(&env, &["deregister", "pets"]);
    assert!(!installed_skill_description(&env).contains("pets"));
    let out = run_ok(&env, &["set-hint", "pets", "old pets"]);
    assert!(out.contains("deregistered"), "{out}");
    assert_eq!(
        read_config(&env)["deregistered"]["pets"]["hint"],
        "old pets"
    );
    run_ok(&env, &["register", "pets"]);
    assert!(installed_skill_description(&env).contains("pets (old pets)"));
    run_ok(&env, &["deregister", "pets"]);
    let out = run_ok(&env, &["register", "pets", "--hint", "pets v2"]);
    assert!(out.contains("Hint      : pets v2"), "{out}");
    assert!(installed_skill_description(&env).contains("other, pets (pets v2))"));

    // 6. --print-skill renders the same labels.
    let printed = run_ok(&env, &["sync-ides", "--print-skill"]);
    assert!(printed.contains("pets (pets v2)"), "{printed}");
    let doctor = run_ok(&env, &["doctor"]);
    assert!(doctor.contains("[UP TO DATE   ] Zed -> "), "{doctor}");
    assert!(
        doctor.contains("[UP TO DATE   ] VS Code + Copilot CLI"),
        "{doctor}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_mcp_media_spool_and_daemon_reload_env() {
    let env = IsolatedEnv::new();
    let base_url = spawn_mock_api().await;
    let spec_url = format!("{}/openapi.json", base_url);

    // Spawn a mock HTTP MCP server returning standard `inputSchema` and an inline base64 PNG image
    let mcp_handler = |Json(payload): Json<Value>| async move {
        let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "initialize" => Json(json!({
                "jsonrpc": "2.0",
                "id": payload.get("id").cloned().unwrap_or(json!(1)),
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mock-mcp", "version": "1.0.0" }
                }
            })),
            "tools/list" => Json(json!({
                "jsonrpc": "2.0",
                "id": payload.get("id").cloned().unwrap_or(json!(1)),
                "result": {
                    "tools": [{
                        "name": "take_shot",
                        "description": "Capture screen and return MCP image block",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "window": { "type": "string" } },
                            "required": ["window"]
                        }
                    }]
                }
            })),
            "tools/call" => {
                // Base64 for b"\x89PNG\r\n\x1a\nhello-png"
                let b64 = "iVBORw0KGgoaaGVsbG8tcG5n";
                let args = payload
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or_default();
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": payload.get("id").cloned().unwrap_or(json!(1)),
                    "result": {
                        "echo_args": args,
                        "content": [
                            { "type": "text", "text": "Captured window" },
                            { "type": "image", "mimeType": "image/png", "data": b64 }
                        ]
                    }
                }))
            }
            _ => Json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
        }
    };

    let mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_addr = mcp_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mcp_listener, Router::new().route("/mcp", post(mcp_handler)))
            .await
            .unwrap();
    });
    let mcp_url = format!("http://{}/mcp", mcp_addr);

    // 1. Import & register the MCP server (`utcp import-mcp`)
    let imp = env
        .cmd()
        .args(["import-mcp", "screen_mcp", "--url", &mcp_url, "--register"])
        .output()
        .unwrap();
    assert!(imp.status.success(), "import-mcp failed: {:?}", imp);

    // Verify lazy schema inspection picked up `inputSchema`
    let schema_out = env
        .cmd()
        .args(["schema", "screen_mcp.take_shot"])
        .output()
        .unwrap();
    assert!(schema_out.status.success());
    let schema_val: Value = serde_json::from_slice(&schema_out.stdout).unwrap();
    assert!(schema_val["inputs"]["properties"]["window"].is_object());

    // issues/012: the cached `.utcp.json` that `import-mcp` wrote must be accepted as a `register`
    // source, both as written now and in the legacy shape (template without `call_template_type`).
    let cached = env.tmp.path().join("cache/manuals/screen_mcp.utcp.json");
    let mut cached_val: Value = serde_json::from_slice(&std::fs::read(&cached).unwrap()).unwrap();
    assert_eq!(
        cached_val["tools"][0]["tool_call_template"]["call_template_type"],
        "mcp"
    );
    let copy_path = env.tmp.path().join("screen_copy.utcp.json");
    std::fs::copy(&cached, &copy_path).unwrap();
    let reg_copy = env
        .cmd()
        .args(["register", "screen_copy", copy_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        reg_copy.status.success(),
        "register from cached manual failed: {:?}",
        reg_copy
    );

    for tool in cached_val["tools"].as_array_mut().unwrap() {
        tool["tool_call_template"]
            .as_object_mut()
            .unwrap()
            .remove("call_template_type");
    }
    let legacy_path = env.tmp.path().join("screen_legacy.utcp.json");
    std::fs::write(&legacy_path, serde_json::to_vec(&cached_val).unwrap()).unwrap();
    let reg_legacy = env
        .cmd()
        .args(["register", "screen_legacy", legacy_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        reg_legacy.status.success(),
        "register from legacy manual failed: {:?}",
        reg_legacy
    );
    let legacy_schema = env
        .cmd()
        .args(["schema", "screen_legacy.take_shot"])
        .output()
        .unwrap();
    assert!(legacy_schema.status.success());

    // 2. Register OpenAPI manual WITHOUT `.env` set yet, then start daemon
    assert!(env
        .cmd()
        .args(["register", "dyn_api", &spec_url])
        .output()
        .unwrap()
        .status
        .success());
    assert!(env
        .cmd()
        .args(["daemon", "start"])
        .output()
        .unwrap()
        .status
        .success());

    // 3. Call MCP tool through running daemon -> inline base64 image must be spooled to `UTCP_MEDIA_DIR`!
    let shot_out = env
        .cmd()
        .args(["call", "screen_mcp.take_shot", r#"{"window": "main"}"#])
        .output()
        .unwrap();
    assert!(shot_out.status.success(), "mcp call failed: {:?}", shot_out);
    let shot_val: Value = serde_json::from_slice(&shot_out.stdout).unwrap();
    assert_eq!(shot_val["echo_args"]["window"], "main");
    assert_eq!(shot_val["content"][1]["type"], "media_file");
    let spooled_path = shot_val["content"][1]["path"].as_str().unwrap();
    assert!(std::path::Path::new(spooled_path).exists());

    // 4a. Cold-boot auto-absorption: calling `utcp call` from a shell that holds `dyn__api_API_KEY`
    //     automatically populates the missing variable in a cold systemd-started daemon!
    //     Unrelated shell variables (even secret-looking ones) must never be absorbed.
    let cold_call = env
        .cmd()
        .env("dyn__api_API_KEY", "cold-boot-shell-secret")
        .env("UNRELATED_SECRET_TOKEN", "must-not-be-kept")
        .args(["call", "dyn_api.get_user", r#"{"user_id": "alice_cold"}"#])
        .output()
        .unwrap();
    assert!(cold_call.status.success());
    let cold_val: Value = serde_json::from_slice(&cold_call.stdout).unwrap();
    assert_eq!(cold_val["auth"], "Bearer cold-boot-shell-secret");

    // 4b. Explicit `utcp daemon reload-env` rotates/overwrites an already-cached token in daemon RAM!
    let reload_out = env
        .cmd()
        .env("dyn__api_API_KEY", "live-pushed-shell-secret")
        .env("ANOTHER_UNRELATED_PASSWORD", "must-not-be-kept-either")
        .args(["daemon", "reload-env"])
        .output()
        .unwrap();
    assert!(
        reload_out.status.success(),
        "reload-env failed: {:?}",
        reload_out
    );

    let authed_call = env
        .cmd()
        .args(["call", "dyn_api.get_user", r#"{"user_id": "bob"}"#])
        .output()
        .unwrap();
    assert!(authed_call.status.success());
    let authed_val: Value = serde_json::from_slice(&authed_call.stdout).unwrap();
    assert_eq!(authed_val["auth"], "Bearer live-pushed-shell-secret");

    // 5. Verify `daemon status` polls do NOT inflate `requests_served`
    let st1 = env
        .cmd()
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    let v1: Value = serde_json::from_slice(&st1.stdout).unwrap();
    let st2 = env
        .cmd()
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    let v2: Value = serde_json::from_slice(&st2.stdout).unwrap();
    assert_eq!(
        v1["requests_served"], v2["requests_served"],
        "background TUI/CLI status polls must not inflate requests_served"
    );

    // 5b. The daemon keeps only variables registered manuals use: `dyn__api_API_KEY` and nothing
    //     from the callers' shells (`UNRELATED_SECRET_TOKEN`, `ANOTHER_UNRELATED_PASSWORD`, `PATH`, ...).
    assert_eq!(
        v2["env_var_count"], 1,
        "daemon must hold only manual env vars: {}",
        v2
    );
    let dyn_cred = v2["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["manual"] == "dyn_api")
        .expect("dyn_api credential row");
    assert_eq!(dyn_cred["is_set"], true);

    // 6. Clear media spool & stop daemon
    let clear_out = env.cmd().args(["media", "clear"]).output().unwrap();
    assert!(clear_out.status.success());
    assert!(!std::path::Path::new(spooled_path).exists());

    let _ = env.cmd().args(["daemon", "stop"]).output().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compact_pretty_fields_projection_and_dynamic_skill_refresh() {
    let env = IsolatedEnv::new();
    let base_url = spawn_mock_api().await;
    let spec_url = format!("{}/openapi.json", base_url);

    // 1. Create `~/.hermes/skills` inside isolated HOME and run initial `sync-ides --global`
    let home = env.tmp.path().join("home");
    std::fs::create_dir_all(home.join(".hermes/skills")).unwrap();
    assert!(env
        .cmd()
        .args(["sync-ides", "--global"])
        .output()
        .unwrap()
        .status
        .success());

    let agy_skill = home.join(".gemini/antigravity-cli/skills/utcp-tools/SKILL.md");
    let hermes_skill = home.join(".hermes/skills/software-development/utcp-tools/SKILL.md");
    assert!(agy_skill.exists());
    assert!(hermes_skill.exists());
    assert!(home
        .join(".hermes/skills/software-development/utcp-admin/operating.md")
        .exists());

    // 2. Register `alpha` and `beta` -> synced global SKILL.md files must auto-update `(registered manuals: alpha, beta)`!
    assert!(env
        .cmd()
        .args(["register", "alpha", &spec_url])
        .output()
        .unwrap()
        .status
        .success());
    assert!(env
        .cmd()
        .args(["register", "beta", &spec_url])
        .output()
        .unwrap()
        .status
        .success());

    let skill_text = std::fs::read_to_string(&agy_skill).unwrap();
    assert!(
        skill_text.contains("registered manuals: alpha, beta"),
        "expected dynamic manual hint in SKILL.md, got:\n{}",
        skill_text
    );

    // Deregistering `beta` updates the hint back to `(registered manuals: alpha)`
    assert!(env
        .cmd()
        .args(["deregister", "beta", "--purge-cache"])
        .output()
        .unwrap()
        .status
        .success());
    let skill_after_dereg = std::fs::read_to_string(&hermes_skill).unwrap();
    assert!(skill_after_dereg.contains("registered manuals: alpha"));
    assert!(!skill_after_dereg.contains("beta"));

    // 3. Verify auto-compact default (1 line when piped) vs `--pretty` (multi-line) + `--fields` projection
    assert!(env
        .cmd()
        .args(["daemon", "start"])
        .output()
        .unwrap()
        .status
        .success());

    let compact_out = env
        .cmd()
        .env("ALPHA_API_KEY", "tok")
        .args([
            "call",
            "alpha.get_user",
            r#"{"user_id": "carol", "verbose": true}"#,
            "--fields",
            "user_id",
        ])
        .output()
        .unwrap();
    assert!(compact_out.status.success());
    let compact_str = String::from_utf8_lossy(&compact_out.stdout);
    assert_eq!(
        compact_str.lines().count(),
        1,
        "piped/agent output must be single-line compact JSON by default"
    );
    let compact_val: Value = serde_json::from_str(compact_str.trim()).unwrap();
    assert_eq!(compact_val, json!({"user_id": "carol"}));

    let pretty_out = env
        .cmd()
        .args([
            "call",
            "alpha.get_user",
            r#"{"user_id": "carol", "verbose": true}"#,
            "--fields",
            "user_id",
            "--pretty",
        ])
        .output()
        .unwrap();
    assert!(pretty_out.status.success());
    let pretty_str = String::from_utf8_lossy(&pretty_out.stdout);
    assert!(
        pretty_str.lines().count() >= 3,
        "--pretty must emit multi-line indented JSON"
    );

    // 4. Verify per-item `"fields"` inside `utcp batch --stdin`
    let mut batch_child = env
        .cmd()
        .args(["batch", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    batch_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            br#"[
                {"id": "1", "tool": "alpha.get_user", "args": {"user_id": "dave", "verbose": true}, "fields": ["user_id"]}
            ]"#,
        )
        .unwrap();
    let batch_out = batch_child.wait_with_output().unwrap();
    assert!(batch_out.status.success());
    let batch_str = String::from_utf8_lossy(&batch_out.stdout);
    assert_eq!(batch_str.lines().count(), 1);
    let batch_val: Value = serde_json::from_str(batch_str.trim()).unwrap();
    assert_eq!(batch_val[0]["result"], json!({"user_id": "dave"}));

    // Piped input is read without `--stdin`; an empty pipe is an error, not an empty batch.
    let mut piped = env
        .cmd()
        .args(["batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    piped
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"[{"tool": "alpha.get_user", "args": {"user_id": "erin"}, "fields": "user_id"}]"#,
        )
        .unwrap();
    let piped_out = piped.wait_with_output().unwrap();
    assert!(piped_out.status.success());
    let piped_val: Value = serde_json::from_slice(&piped_out.stdout).unwrap();
    assert_eq!(piped_val[0]["result"], json!({"user_id": "erin"}));
    let empty = env
        .cmd()
        .args(["batch"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!empty.status.success());
    assert!(
        String::from_utf8_lossy(&empty.stderr).contains("read nothing"),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );

    let _ = env.cmd().args(["daemon", "stop"]).output().unwrap();
}

/// `import-mcp` resolves `${VAR}` placeholders for discovery (the saved manual keeps them), names
/// unset ones when discovery fails, infers a name, accepts `--arg -y`, and explains bad commands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_mcp_resolves_placeholders_and_reports_clear_errors() {
    use axum::http::StatusCode;
    let env = IsolatedEnv::new();

    let mcp_handler = |headers: HeaderMap, Json(payload): Json<Value>| async move {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != "Bearer tok-123" {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "bad token"})),
            );
        }
        let id = payload.get("id").cloned().unwrap_or(json!(1));
        let result = match payload.get("method").and_then(|v| v.as_str()) {
            Some("initialize") => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "auth-mcp", "version": "1.0.0" }
            }),
            Some("tools/list") => json!({
                "tools": [{ "name": "ping", "description": "Ping", "inputSchema": { "type": "object" } }]
            }),
            _ => json!({}),
        };
        (
            StatusCode::OK,
            Json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/mcp", post(mcp_handler)))
            .await
            .unwrap();
    });
    let mcp_url = format!("http://{}/mcp", addr);
    let auth = "Authorization: Bearer ${MOCK_MCP_TOKEN}";

    // 1. Token not set anywhere: discovery fails and names the missing placeholder.
    let err = run_err(
        &env,
        &[
            "import-mcp",
            "authy",
            "--url",
            &mcp_url,
            "-H",
            auth,
            "--register",
        ],
    );
    assert!(err.contains("MOCK_MCP_TOKEN"), "{err}");

    // 2. Token in the utcp .env: discovery succeeds, the saved manual keeps the placeholder.
    std::fs::create_dir_all(env.tmp.path().join("config")).unwrap();
    std::fs::write(
        env.tmp.path().join("config/.env"),
        "MOCK_MCP_TOKEN=tok-123\n",
    )
    .unwrap();
    let out = run_ok(
        &env,
        &[
            "import-mcp",
            "authy",
            "--url",
            &mcp_url,
            "-H",
            auth,
            "--register",
            "--hint",
            "auth  test\nserver",
        ],
    );
    assert!(out.contains("Secrets   : MOCK_MCP_TOKEN"), "{out}");
    assert!(out.contains("Hint      : auth test server"), "{out}");
    assert_eq!(
        read_config(&env)["manuals"]["authy"]["hint"],
        "auth test server"
    );
    let saved =
        std::fs::read_to_string(env.tmp.path().join("cache/manuals/authy.utcp.json")).unwrap();
    assert!(saved.contains("${MOCK_MCP_TOKEN}"));
    assert!(
        !saved.contains("tok-123"),
        "secret value leaked into the manual"
    );
    assert!(run_ok(&env, &["list", "authy"]).contains("authy.ping"));

    // 3. No name: inferred (here from the 127.0.0.1 host); `-o` writes a file without registering.
    let file = env.tmp.path().join("inferred.utcp.json");
    let out = run_ok(
        &env,
        &[
            "import-mcp",
            "--url",
            &mcp_url,
            "-H",
            auth,
            "-o",
            file.to_str().unwrap(),
        ],
    );
    assert!(out.contains("Manual    : 127"), "{out}");
    assert!(file.exists());

    // 4. `--arg -y` parses (hyphenated values), and a missing program is named in the error.
    let err = run_err(
        &env,
        &[
            "import-mcp",
            "ghost",
            "--command",
            "/nonexistent/utcp-mcp-ghost",
            "--arg",
            "-y",
            "--arg",
            "@scope/ghost",
        ],
    );
    assert!(
        err.contains("failed to start MCP server command '/nonexistent/utcp-mcp-ghost'"),
        "{err}"
    );

    // 5. A whole command line in --command gets a hint to use --arg.
    let err = run_err(
        &env,
        &["import-mcp", "ghost", "--command", "npx -y @scope/ghost"],
    );
    assert!(
        err.contains("try: --command npx --arg -y --arg @scope/ghost"),
        "{err}"
    );
}

/// Issue 013 follow-up: `register`, `import` and `refresh` resolve `${VAR}` in `-H` headers when
/// they download the spec itself (the saved manual keeps the placeholder), and a failed download
/// names any placeholder that was not set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_fetch_resolves_header_placeholders() {
    use axum::http::StatusCode;
    let env = IsolatedEnv::new();

    let spec_handler = |headers: HeaderMap| async move {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != "Bearer tok-spec" {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "bad token"})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({
                "openapi": "3.0.3",
                "info": { "title": "Private Spec", "version": "1.0.0" },
                "servers": [{ "url": "http://127.0.0.1:9" }],
                "paths": {
                    "/ping": {
                        "get": {
                            "operationId": "ping",
                            "summary": "Ping",
                            "responses": { "200": { "description": "pong" } }
                        }
                    }
                }
            })),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/spec.json", get(spec_handler)),
        )
        .await
        .unwrap();
    });
    let spec_url = format!("http://{}/spec.json", addr);
    let auth = "Authorization: Bearer ${MOCK_SPEC_TOKEN}";

    // 1. Token not set: the download fails and the error names the placeholder.
    let err = run_err(&env, &["register", "privy", &spec_url, "-H", auth]);
    assert!(err.contains("HTTP 401"), "{err}");
    assert!(err.contains("MOCK_SPEC_TOKEN"), "{err}");

    // 2. Token in the utcp .env: register works and nothing saved holds the secret.
    std::fs::create_dir_all(env.tmp.path().join("config")).unwrap();
    std::fs::write(
        env.tmp.path().join("config/.env"),
        "MOCK_SPEC_TOKEN=tok-spec\n",
    )
    .unwrap();
    run_ok(&env, &["register", "privy", &spec_url, "-H", auth]);
    assert!(run_ok(&env, &["list", "privy"]).contains("privy.ping"));
    let config = read_config(&env);
    assert_eq!(
        config["manuals"]["privy"]["headers"]["Authorization"],
        "Bearer ${MOCK_SPEC_TOKEN}"
    );
    let config_text =
        std::fs::read_to_string(env.tmp.path().join("config/.utcp_config.json")).unwrap();
    assert!(!config_text.contains("tok-spec"), "secret saved in config");
    for dir in ["cache/manuals", "config/manuals"] {
        let Ok(entries) = std::fs::read_dir(env.tmp.path().join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                !text.contains("tok-spec"),
                "secret saved in {:?}",
                entry.path()
            );
        }
    }

    // 3. `refresh` downloads again with the stored placeholder header.
    run_ok(&env, &["refresh", "privy"]);

    // 4. `import -o` (no registration) resolves it too; the written file keeps the placeholder.
    let file = env.tmp.path().join("privy.utcp.json");
    run_ok(
        &env,
        &[
            "import",
            &spec_url,
            "-H",
            auth,
            "-o",
            file.to_str().unwrap(),
        ],
    );
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(
        !written.contains("tok-spec"),
        "secret saved in the import file"
    );

    // 5. The secrets command is asked for the placeholders of a manual being registered (not in
    //    the index yet), with no `.env` value to fall back on.
    std::fs::remove_file(env.tmp.path().join("config/.env")).unwrap();
    let runs = configure_secret_source(&env, &json!({"values": {"OTHER_SPEC_TOKEN": "tok-spec"}}));
    run_ok(
        &env,
        &[
            "register",
            "privy_two",
            &spec_url,
            "-H",
            "Authorization: Bearer ${OTHER_SPEC_TOKEN}",
        ],
    );
    let asked = std::fs::read_to_string(runs).unwrap();
    assert!(asked.contains("OTHER_SPEC_TOKEN"), "{asked}");
}

/// issues/014 end to end: a real stdio MCP server (`node`, the shared fixture) registered with
/// `import-mcp --command`, then called through the daemon by many concurrent `utcp call`
/// processes. Each caller gets its own answer from one shared server, and after the server
/// crashes the next call starts a new one. The command is an absolute path in a folder with a
/// space (and on Windows parentheses, like `Program Files (x86)`), as installed tools often are.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_mcp_server_serves_concurrent_daemon_calls_and_restarts_after_crash() {
    let env = Arc::new(IsolatedEnv::new());
    let script = mock_mcp_script();
    let node = node_in_folder_with_awkward_name(&env);
    run_ok(
        &env,
        &[
            "import-mcp",
            "nodemock",
            "--command",
            &node,
            "--arg",
            &script,
            "--register",
        ],
    );
    run_ok(&env, &["daemon", "start"]);

    let callers: Vec<_> = (0..30)
        .map(|tag| {
            let env = Arc::clone(&env);
            std::thread::spawn(move || {
                let out = run_ok(
                    &env,
                    &["call", "nodemock.echo", &format!(r#"{{"tag":{tag}}}"#)],
                );
                (tag, serde_json::from_str::<Value>(&out).unwrap())
            })
        })
        .collect();
    let mut pids = std::collections::HashSet::new();
    for handle in callers {
        let (tag, v) = handle.join().unwrap();
        assert_eq!(v["tag"], tag, "caller {tag} got someone else's answer: {v}");
        pids.insert(v["pid"].as_u64().unwrap());
    }
    assert_eq!(
        pids.len(),
        1,
        "the daemon keeps one server for all callers: {pids:?}"
    );
    let first_pid = pids.into_iter().next().unwrap();

    let stats: Value =
        serde_json::from_str(&run_ok(&env, &["call", "nodemock.stats", "{}"])).unwrap();
    assert_eq!(stats["pingsAnswered"], 30, "{stats}");

    let err = run_err(&env, &["call", "nodemock.crash", "{}"]);
    assert!(err.contains("'nodemock' closed its output"), "{err}");

    let after: Value =
        serde_json::from_str(&run_ok(&env, &["call", "nodemock.echo", r#"{"tag":99}"#])).unwrap();
    assert_eq!(after["tag"], 99);
    assert_ne!(
        after["pid"].as_u64().unwrap(),
        first_pid,
        "crashed server not replaced"
    );

    run_ok(&env, &["daemon", "stop"]);
}

fn process_running(pid: u64) -> bool {
    #[cfg(unix)]
    {
        // A zombie (exited, not yet reaped) counts as stopped.
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            return !state.starts_with('Z');
        }
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("run tasklist");
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
}

/// Every server start so far, from the fixture's `MOCK_MCP_PID_FILE`.
fn started_pids(file: &std::path::Path) -> Vec<u64> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn assert_all_stopped(file: &std::path::Path, when: &str) {
    for _ in 0..250 {
        if !started_pids(file).into_iter().any(process_running) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let running: Vec<_> = started_pids(file)
        .into_iter()
        .filter(|p| process_running(*p))
        .collect();
    panic!("MCP server still running {when}: {running:?}");
}

/// A server started through a launcher (like `npx`), which ignores stdin closing and SIGTERM,
/// is stopped with its whole process tree after `import-mcp` discovery, on `deregister` (daemon
/// reload) and on `daemon stop`. On Unix the SIGKILL comes after a grace period, so this also
/// checks that the CLI and the daemon wait for it before exiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_mcp_server_tree_is_stopped_after_import_deregister_and_daemon_stop() {
    let env = IsolatedEnv::new();
    let script = mock_mcp_script();
    let pid_file = env.tmp.path().join("server-pids.txt");
    let pid_env = format!("MOCK_MCP_PID_FILE={}", pid_file.display());
    run_ok(
        &env,
        &[
            "import-mcp",
            "stubborn",
            "--command",
            "node",
            "--arg",
            &script,
            "--arg",
            "--wrapper",
            "--arg",
            "--stubborn",
            "--arg",
            "--ignore-term",
            "-e",
            &pid_env,
            "--register",
        ],
    );
    assert_eq!(
        started_pids(&pid_file).len(),
        1,
        "discovery started one server"
    );
    assert_all_stopped(&pid_file, "after import-mcp");

    run_ok(&env, &["daemon", "start"]);
    let call = |tag: u64| -> u64 {
        let out = run_ok(
            &env,
            &["call", "stubborn.echo", &format!(r#"{{"tag":{tag}}}"#)],
        );
        serde_json::from_str::<Value>(&out).unwrap()["pid"]
            .as_u64()
            .unwrap()
    };
    let pid = call(1);
    assert!(process_running(pid));
    run_ok(&env, &["deregister", "stubborn"]);
    assert_all_stopped(&pid_file, "after deregister");

    run_ok(&env, &["register", "stubborn"]);
    let pid = call(2);
    assert!(process_running(pid));
    run_ok(&env, &["daemon", "stop"]);
    assert_all_stopped(&pid_file, "after daemon stop");
}

/// `refresh` on an `import-mcp` manual asks the MCP server again: a tool the server added since
/// the import becomes callable, and the manual's hint survives.
#[test]
fn refresh_asks_the_mcp_server_again() {
    let env = IsolatedEnv::new();
    let script = mock_mcp_script();
    let extra_file = env.tmp.path().join("extra-tools.txt");
    let extra_env = format!("MOCK_MCP_EXTRA_TOOLS_FILE={}", extra_file.display());
    let out = run_ok(
        &env,
        &[
            "import-mcp",
            "growing",
            "--command",
            "node",
            "--arg",
            &script,
            "-e",
            &extra_env,
            "--register",
            "--hint",
            "grows over time",
        ],
    );
    assert!(out.contains("Tools     : 4"), "{out}");

    std::fs::write(&extra_file, "fresh_tool").unwrap();
    let out = run_ok(&env, &["refresh", "growing"]);
    assert!(out.contains("Refreshed 'growing' (5 tools"), "{out}");
    let answer = run_ok(&env, &["call", "growing.fresh_tool", r#"{"tag":7}"#]);
    assert_eq!(serde_json::from_str::<Value>(&answer).unwrap()["tag"], 7);
    let config = std::fs::read_to_string(env.tmp.path().join("config/.utcp_config.json")).unwrap();
    assert!(config.contains("grows over time"), "{config}");
}

/// `UTCP_TEST_FIXTURES` (a folder holding the fixtures) overrides `utcp-core/tests/fixtures`.
fn mock_mcp_script() -> String {
    fixture_path("mock_mcp_server.cjs")
}

fn fixture_path(name: &str) -> String {
    std::env::var_os("UTCP_TEST_FIXTURES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../utcp-core/tests/fixtures")
        })
        .join(name)
        .display()
        .to_string()
}

/// Writes `settings.toml` with a `[secrets]` command running `secret_source.cjs` on `config`, and
/// returns the file the fixture appends the requested names to.
fn configure_secret_source(env: &IsolatedEnv, config: &Value) -> std::path::PathBuf {
    let config_dir = env.tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let source_config = env.tmp.path().join("secrets.json");
    std::fs::write(&source_config, config.to_string()).unwrap();
    let settings = format!(
        "[secrets]\ncommand = [\"node\", {}, {}]\ntimeout_secs = 5\n",
        toml_string(&fixture_path("secret_source.cjs")),
        toml_string(&source_config.display().to_string()),
    );
    std::fs::write(config_dir.join("settings.toml"), settings).unwrap();
    env.tmp.path().join("secrets.json.runs")
}

/// A TOML literal string (no escapes, so Windows paths stay as they are).
fn toml_string(s: &str) -> String {
    assert!(!s.contains('\''), "{s}");
    format!("'{s}'")
}

fn doctor_json(env: &IsolatedEnv) -> Value {
    let out = run_ok(env, &["doctor", "--json"]);
    serde_json::from_str::<Value>(&out).unwrap()["doctor"].clone()
}

fn credential<'a>(doctor: &'a Value, manual: &str) -> &'a Value {
    doctor["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["manual"] == manual)
        .unwrap_or_else(|| panic!("no credential row for {manual}: {doctor}"))
}

fn call_auth(env: &IsolatedEnv) -> Value {
    let out = run_ok(env, &["call", "dyn_api.get_user", r#"{"user_id": "x"}"#]);
    serde_json::from_str::<Value>(&out).unwrap()["auth"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_command_supplies_credentials_to_the_daemon_and_direct_mode() {
    let base_url = spawn_mock_api().await;
    let env = IsolatedEnv::new();
    let runs = configure_secret_source(
        &env,
        &json!({"values": {"dyn__api_API_KEY": "from-secrets-command"}}),
    );
    run_ok(
        &env,
        &["register", "dyn_api", &format!("{base_url}/openapi.json")],
    );

    // Direct mode: the command fills the placeholder, and doctor says where the value came from.
    assert_eq!(call_auth(&env), "Bearer from-secrets-command");
    let doctor = doctor_json(&env);
    assert_eq!(credential(&doctor, "dyn_api")["source"], "secrets command");
    assert_eq!(doctor["secrets_command"]["error"], Value::Null);

    // The daemon runs it at startup: no caller pushes the value (the test shell does not have it).
    run_ok(&env, &["daemon", "start"]);
    assert_eq!(call_auth(&env), "Bearer from-secrets-command");
    let status: Value =
        serde_json::from_str(&run_ok(&env, &["daemon", "status", "--json"])).unwrap();
    assert_eq!(
        status["env_var_count"], 1,
        "UNASKED_SECRET must not be held: {status}"
    );
    assert_eq!(
        status["secrets_command"]["provided"],
        json!(["dyn__api_API_KEY"])
    );
    let doctor = doctor_json(&env);
    assert_eq!(credential(&doctor, "dyn_api")["source"], "secrets command");

    // It is asked only for names registered manuals may read.
    let asked = std::fs::read_to_string(&runs).unwrap();
    assert!(asked.lines().count() >= 2, "{asked}");
    for line in asked.lines() {
        assert!(line.split(' ').any(|n| n == "dyn__api_API_KEY"), "{asked}");
        assert!(
            line.split(' ')
                .all(|n| n.to_ascii_lowercase().contains("api")),
            "{asked}"
        );
    }

    // `reload-env` runs it again, so a rotated secret is picked up without a restart.
    let source_config = env.tmp.path().join("secrets.json");
    std::fs::write(
        &source_config,
        json!({"values": {"dyn__api_API_KEY": "rotated"}}).to_string(),
    )
    .unwrap();
    let out = run_ok(&env, &["daemon", "reload-env"]);
    assert!(out.contains("Secrets command re-run"), "{out}");
    assert_eq!(call_auth(&env), "Bearer rotated");

    // A failing run keeps the last good values, and the error (never the output) is reported.
    std::fs::write(&source_config, json!({"exit": 3}).to_string()).unwrap();
    let out = run_ok(&env, &["daemon", "reload-env"]);
    assert!(
        out.contains("Secrets command failed: exited with code 3"),
        "{out}"
    );
    assert!(!out.contains("SECRET-VALUE"), "{out}");
    assert_eq!(call_auth(&env), "Bearer rotated");

    // A daemon whose command fails still starts; doctor and daemon.log say why.
    run_ok(&env, &["daemon", "stop"]);
    run_ok(&env, &["daemon", "start"]);
    let doctor = doctor_json(&env);
    let error = doctor["secrets_command"]["error"].as_str().unwrap();
    assert!(error.starts_with("exited with code 3"), "{doctor}");
    assert_eq!(credential(&doctor, "dyn_api")["is_set"], false);
    let log = std::fs::read_to_string(env.tmp.path().join("state/daemon.log")).unwrap();
    assert!(log.contains("failed: exited with code 3"), "{log}");
    assert!(log.contains("dyn__api_API_KEY (dyn_api)"), "{log}");
    assert!(!log.contains("SECRET-VALUE"), "{log}");

    // An invalid settings.toml is reported instead of silently ignored.
    std::fs::write(env.tmp.path().join("config/settings.toml"), "[secrets\n").unwrap();
    let text = run_ok(&env, &["doctor"]);
    assert!(text.contains("Settings          : NOT USED"), "{text}");

    run_ok(&env, &["daemon", "stop"]);
}

/// `node` from `PATH`, linked into `<tmp>/My Tools (x86)/` (no parentheses on Unix, where the command
/// check refuses them), and returned as that absolute path.
fn node_in_folder_with_awkward_name(env: &IsolatedEnv) -> String {
    let file = if cfg!(windows) { "node.exe" } else { "node" };
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(file))
        .find(|p| p.is_file())
        .expect("node on PATH");
    let dir = env.tmp.path().join(if cfg!(windows) {
        "My Tools (x86)"
    } else {
        "My Tools"
    });
    std::fs::create_dir_all(&dir).unwrap();
    let linked = dir.join(file);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&found, &linked).unwrap();
    #[cfg(windows)]
    if std::fs::hard_link(&found, &linked).is_err() {
        std::fs::copy(&found, &linked).unwrap();
    }
    linked.display().to_string()
}

/// On Windows a bare `--command` name finds a `.cmd` launcher on `PATH`, as `npx` (`npx.cmd`)
/// needs. Rust's `Command` alone only looks for `mocknode.exe`. A manual that sets its own `PATH`
/// (`-e PATH=...`) is looked up on that `PATH`.
#[cfg(windows)]
#[test]
fn bare_command_name_finds_cmd_launcher_on_windows_path() {
    let env = IsolatedEnv::new();
    let orig_path = std::env::var_os("PATH").unwrap_or_default();
    let node = std::env::split_paths(&orig_path)
        .map(|dir| dir.join("node.exe"))
        .find(|p| p.is_file())
        .expect("node on PATH");
    let path_with = |dir: &std::path::Path| {
        let mut dirs = vec![dir.to_path_buf()];
        dirs.extend(std::env::split_paths(&orig_path));
        std::env::join_paths(dirs).unwrap()
    };
    let launcher = |folder: &str, name: &str| {
        let dir = env.tmp.path().join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.cmd")),
            format!("@\"{}\" %*\r\n", node.display()),
        )
        .unwrap();
        dir
    };
    // `mocknode` is on utcp's own PATH; `ownnode` only on the PATH the manual sets.
    let utcp_path = path_with(&launcher("bin", "mocknode"));
    let own_path = path_with(&launcher("own", "ownnode"));
    let own_env = format!("PATH={}", own_path.to_string_lossy());
    let script = mock_mcp_script();
    let run = |args: &[&str]| -> String {
        let out = env
            .cmd()
            .env("PATH", &utcp_path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "utcp {:?} failed:\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    run(&[
        "import-mcp",
        "cmdmock",
        "--command",
        "mocknode",
        "--arg",
        &script,
        "--register",
    ]);
    run(&[
        "import-mcp",
        "ownpath",
        "--command",
        "ownnode",
        "--arg",
        &script,
        "-e",
        &own_env,
        "--register",
    ]);
    for server in ["cmdmock", "ownpath"] {
        for mode in [&["--direct"][..], &[][..]] {
            let tool = format!("{server}.echo");
            let mut args = mode.to_vec();
            args.extend(["call", tool.as_str(), r#"{"tag":1}"#]);
            let out: Value = serde_json::from_str(&run(&args)).unwrap();
            assert_eq!(out["tag"], 1, "{server} {mode:?}: {out}");
        }
    }
    run(&["daemon", "stop"]);
}

fn wait_until_stopped(pid: u64, what: &str) {
    for _ in 0..250 {
        if !process_running(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("{what} ({pid}) is still running");
}

/// Stopping a direct-mode `utcp call` (Ctrl+C, or SIGTERM from a supervisor) stops the MCP
/// server it started right away, with its whole tree, and exits with 128 + signal. The server
/// ignores stdin closing, so only utcp's own stop can end it.
#[cfg(unix)]
#[test]
fn interrupted_direct_call_stops_its_mcp_server() {
    let env = IsolatedEnv::new();
    let pid_file = env.tmp.path().join("server-pids.txt");
    let pid_env = format!("MOCK_MCP_PID_FILE={}", pid_file.display());
    let script = mock_mcp_script();
    run_ok(
        &env,
        &[
            "import-mcp",
            "direct",
            "--command",
            "node",
            "--arg",
            &script,
            "--arg",
            "--wrapper",
            "--arg",
            "--stubborn",
            "-e",
            &pid_env,
            "--register",
        ],
    );
    assert_all_stopped(&pid_file, "after import-mcp");

    for (signal, code) in [("TERM", 143), ("INT", 130)] {
        let starts = started_pids(&pid_file).len();
        let child = env
            .cmd()
            .args(["--direct", "call", "direct.silent", "{}"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        for _ in 0..250 {
            if started_pids(&pid_file).len() > starts {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let server = *started_pids(&pid_file).last().unwrap();
        assert_eq!(
            started_pids(&pid_file).len(),
            starts + 1,
            "server not started"
        );
        // Give the started server a moment to receive the call.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(process_running(server));

        let sent = Command::new("kill")
            .args([&format!("-{signal}"), &child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "SIG{signal}: {stderr}");
        assert!(
            stderr.contains(&format!("interrupted (SIG{signal})")),
            "{stderr}"
        );
        // utcp waits for the stop before exiting, so the server is already gone.
        assert!(!process_running(server), "server survived SIG{signal}");
    }
}

/// `utcp daemon servers / stop-server / restart-server` end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_lists_stops_and_restarts_local_mcp_servers() {
    let env = Arc::new(IsolatedEnv::new());
    let script = mock_mcp_script();
    let pid_file = env.tmp.path().join("server-pids.txt");
    let pid_env = format!("MOCK_MCP_PID_FILE={}", pid_file.display());
    run_ok(
        &env,
        &[
            "import-mcp",
            "nodemock",
            "--command",
            "node",
            "--arg",
            &script,
            "-e",
            &pid_env,
            "--register",
        ],
    );
    let base_url = spawn_mock_api().await;
    run_ok(
        &env,
        &["register", "my_api", &format!("{}/openapi.json", base_url)],
    );
    let echo = || -> u64 {
        let out = run_ok(&env, &["call", "nodemock.echo", r#"{"tag":1}"#]);
        serde_json::from_str::<Value>(&out).unwrap()["pid"]
            .as_u64()
            .unwrap()
    };
    let servers_json = || -> Value {
        serde_json::from_str(&run_ok(&env, &["daemon", "servers", "--json"])).unwrap()
    };

    // 1. Without a daemon there is nothing to manage.
    let err = run_err(&env, &["daemon", "servers"]);
    assert!(err.contains("utcp daemon is not running"), "{err}");

    run_ok(&env, &["daemon", "start"]);

    // 2. Listed before the first call; manuals without a local server are not listed.
    let out = run_ok(&env, &["daemon", "servers"]);
    assert!(out.contains("MANUAL") && out.contains("RESTARTS"), "{out}");
    let row = out
        .lines()
        .find(|l| l.starts_with("nodemock "))
        .expect(&out);
    assert!(
        row.contains("not started") && row.ends_with("  node"),
        "{row}"
    );
    assert!(!out.contains("my_api"), "{out}");

    // 3. Running: pid, uptime, no restarts; the command only, never its arguments.
    let first = echo();
    let list = servers_json();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["manual"], "nodemock");
    assert_eq!(list[0]["state"], "running");
    assert_eq!(list[0]["pid"], first);
    assert_eq!(list[0]["restarts"], 0);
    assert_eq!(list[0]["command"], "node");
    assert!(list[0]["uptime_secs"].is_u64());
    assert!(!list.to_string().contains("mock_mcp_server"), "{list}");

    // 4. stop-server: a waiting call fails at once, the process goes away.
    let waiting = {
        let env = Arc::clone(&env);
        std::thread::spawn(move || run_err(&env, &["call", "nodemock.silent", "{}"]))
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    let out = run_ok(&env, &["daemon", "stop-server", "nodemock"]);
    assert!(
        out.contains("Stopped the MCP server of 'nodemock'"),
        "{out}"
    );
    let err = waiting.join().unwrap();
    assert!(err.contains("'nodemock' was stopped on request"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    wait_until_stopped(first, "stopped server");
    assert_eq!(servers_json()[0]["state"], "stopped");
    let out = run_ok(&env, &["daemon", "stop-server", "nodemock"]);
    assert!(out.contains("was not running"), "{out}");

    // 5. The next call starts it again.
    let second = echo();
    assert_ne!(second, first);
    assert_eq!(servers_json()[0]["restarts"], 1);

    // 6. restart-server: a new process now, the old one stopped, calls go to the new one.
    let out = run_ok(&env, &["daemon", "restart-server", "nodemock"]);
    assert!(
        out.contains("Restarted the MCP server of 'nodemock' (PID "),
        "{out}"
    );
    wait_until_stopped(second, "restarted server");
    let third = echo();
    assert!(out.contains(&format!("(PID {third})")), "{out}");
    assert_eq!(servers_json()[0]["restarts"], 2);

    // 7. Clear errors for manuals without a local server and unknown manuals.
    let err = run_err(&env, &["daemon", "stop-server", "my_api"]);
    assert!(err.contains("'my_api' has no local MCP server"), "{err}");
    let err = run_err(&env, &["daemon", "restart-server", "nope"]);
    assert!(err.contains("'nope' is not registered"), "{err}");

    // 8. Stop and restart are in the recent-calls list.
    let recent = run_ok(&env, &["log", "recent"]);
    assert!(recent.contains("stop-server nodemock"), "{recent}");
    assert!(recent.contains("restart-server nodemock"), "{recent}");

    run_ok(&env, &["daemon", "stop"]);
    assert_all_stopped(&pid_file, "after daemon stop");
}

/// A daemon socket this process may not open (another account's, e.g. root's through sudo) is a
/// clear error, never a quiet fall back to direct mode, "not running", or a second daemon.
#[cfg(unix)]
#[test]
fn daemon_this_process_may_not_use_is_a_clear_error() {
    use std::os::unix::fs::PermissionsExt;

    let env = IsolatedEnv::new();
    run_ok(&env, &["daemon", "start"]);
    let sock = env.tmp.path().join("utcp-test.sock");
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        // Running as root: permissions do not stop us, so there is nothing to test.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
        run_ok(&env, &["daemon", "stop"]);
        return;
    }

    for args in [
        &["daemon", "status"][..],
        &["daemon", "start"],
        &["daemon", "stop"],
        &["search", "anything"],
    ] {
        let err = run_err(&env, args);
        assert!(
            err.contains("Cannot use the utcp daemon") && err.contains("permission denied"),
            "utcp {args:?}: {err}"
        );
    }
    run_ok(&env, &["--direct", "search", "anything"]);

    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    let status = run_ok(&env, &["daemon", "status"]);
    assert!(status.contains("RUNNING"), "{status}");
    run_ok(&env, &["daemon", "stop"]);
}

/// A detached daemon's stderr is discarded, so the error it exits with goes to `daemon.log`, and
/// `daemon start` shows it instead of only "timed out". The socket path sits under a regular file, so
/// the daemon cannot listen (on Windows the path is not a pipe name, which fails the same way).
#[test]
fn daemon_that_cannot_start_says_why_through_daemon_log() {
    let env = IsolatedEnv::new();
    let blocker = env.tmp.path().join("not-a-dir");
    std::fs::write(&blocker, "").unwrap();
    let socket = blocker.join("utcp.sock");
    let start = |args: &[&str]| {
        env.cmd()
            .env("UTCP_SOCKET_PATH", &socket)
            .args(args)
            .output()
            .unwrap()
    };

    let out = start(&["daemon", "start"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "daemon start should fail: {err}");
    assert!(err.contains("exited before listening"), "{err}");
    assert!(err.contains("daemon.log"), "{err}");
    assert!(err.contains("daemon exited with an error"), "{err}");

    let log = std::fs::read_to_string(env.tmp.path().join("state/daemon.log")).unwrap();
    assert!(log.contains("daemon exited with an error"), "{log}");

    let status = String::from_utf8_lossy(&start(&["daemon", "status"]).stdout).into_owned();
    assert!(status.contains("STOPPED"), "{status}");
    assert!(status.contains("Last daemon error or crash"), "{status}");
}

/// Several `daemon run` started at the same instant: exactly one keeps running and answers, the
/// others say a daemon is already running and exit. Before the start lock, every one of them bound
/// the socket (each deleting the previous one's file) and stayed running, unreachable.
#[test]
fn simultaneous_daemon_starts_leave_exactly_one_daemon() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let env = IsolatedEnv::new();
    let mut children: Vec<std::process::Child> = (0..6)
        .map(|_| {
            env.cmd()
                .args(["daemon", "run"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited: Vec<(u32, std::process::ExitStatus, String)> = Vec::new();
    while Instant::now() < deadline && children.len() > 1 {
        let mut i = 0;
        while i < children.len() {
            if let Some(status) = children[i].try_wait().unwrap() {
                let mut child = children.remove(i);
                let _ = child.wait();
                let mut err = String::new();
                use std::io::Read;
                let _ = child.stderr.take().unwrap().read_to_string(&mut err);
                exited.push((child.id(), status, err));
            } else {
                i += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // A loser that has not exited yet would show up here too.
    std::thread::sleep(Duration::from_millis(300));
    let still_running: Vec<u32> = children
        .iter_mut()
        .filter_map(|c| c.try_wait().unwrap().is_none().then(|| c.id()))
        .collect();
    let survivors = still_running.len();
    if survivors != 1 {
        for c in children.iter_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    assert_eq!(survivors, 1, "daemons still running: {still_running:?}");
    for (pid, status, err) in &exited {
        assert!(!status.success(), "daemon {pid} exited 0");
        assert!(err.contains("already running"), "daemon {pid}: {err}");
    }

    let status = run_ok(&env, &["daemon", "status"]);
    assert!(status.contains("RUNNING"), "{status}");
    assert!(
        status.contains(&format!(": {}\n", still_running[0])),
        "status should name PID {}: {status}",
        still_running[0]
    );
    run_ok(&env, &["daemon", "stop"]);
    let survivor = children
        .iter_mut()
        .find(|c| c.id() == still_running[0])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while survivor.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        survivor.try_wait().unwrap().is_some(),
        "daemon did not stop"
    );
    assert!(!env.tmp.path().join("state/daemon.pid").exists());
    #[cfg(unix)]
    assert!(!env.tmp.path().join("utcp-test.sock").exists());
}

/// An OpenAPI spec served by `spawn_mock_api` (bearer auth, so the manual needs `<name>_API_KEY`), with one GET per
/// `(operationId, path, summary)`.
fn spec_file(env: &IsolatedEnv, file: &str, base_url: &str, ops: &[(&str, &str, &str)]) -> String {
    let mut paths = serde_json::Map::new();
    for (id, path, summary) in ops {
        let params: Vec<Value> = path
            .split('/')
            .filter_map(|p| p.strip_prefix('{')?.strip_suffix('}'))
            .map(|p| json!({"name": p, "in": "path", "required": true, "schema": {"type": "string"}}))
            .collect();
        paths.insert(
            path.to_string(),
            json!({"get": {"operationId": id, "summary": summary, "parameters": params,
                           "responses": {"200": {"description": "ok"}}}}),
        );
    }
    let spec = json!({
        "openapi": "3.0.3",
        "info": {"title": "Pets", "version": "1.0.0"},
        "servers": [{"url": base_url}],
        "components": {"securitySchemes": {"bearerAuth": {"type": "http", "scheme": "bearer"}}},
        "security": [{"bearerAuth": []}],
        "paths": paths,
    });
    let path = env.tmp.path().join(file);
    std::fs::write(&path, spec.to_string()).unwrap();
    path.display().to_string()
}

fn call_staged_auth(env: &IsolatedEnv) -> Value {
    let out = run_ok(
        env,
        &["call", "--staged", "pets.get_user", r#"{"user_id": "x"}"#],
    );
    serde_json::from_str::<Value>(&out).unwrap()["auth"].clone()
}

/// `stage` keeps a manual out of search, list and the skills while `--staged` calls reach it (direct and daemon,
/// with the secrets command); `diff`, `promote`, `rollback` (twice undoes it), `discard`, and purge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_diff_promote_rollback_and_discard() {
    let base_url = spawn_mock_api().await;
    let env = IsolatedEnv::new();
    configure_secret_source(&env, &json!({"values": {"pets_API_KEY": "from-secrets"}}));
    std::fs::create_dir_all(env.tmp.path().join("home")).unwrap();
    run_ok(&env, &["sync-ides", "--global"]);
    let v1 = spec_file(
        &env,
        "v1.json",
        &base_url,
        &[
            ("get_user", "/users/{user_id}", "Fetch user"),
            ("old_tool", "/old", "Old"),
        ],
    );
    let v2 = spec_file(
        &env,
        "v2.json",
        &base_url,
        &[
            ("get_user", "/users/{user_id}", "Fetch a user by id"),
            ("new_tool", "/new", "New"),
        ],
    );

    // 1. Staged: not active anywhere, but callable with --staged in direct mode, with the secret.
    let out = run_ok(&env, &["stage", "pets", &v1, "--hint", "pet store"]);
    assert!(out.contains("Staged 'pets' (2 tools"), "{out}");
    assert!(out.contains("Replaces  : nothing"), "{out}");
    assert!(env
        .tmp
        .path()
        .join("cache/manuals/staged/pets.utcp.json")
        .exists());
    assert!(!run_ok(&env, &["list", "--tools"]).contains("pets."));
    assert!(!run_ok(&env, &["search", "user"]).contains("pets."));
    assert!(run_ok(&env, &["list", "--staged", "--tools"]).contains("pets.get_user"));
    assert!(run_ok(&env, &["list", "pets", "--staged"]).contains("pets.get_user"));
    let out = run_ok(&env, &["search", "--staged", "user"]);
    assert!(out.contains("[staged] Fetch user"), "{out}");
    let err = run_err(&env, &["list", "pets"]);
    assert!(err.contains("utcp list pets --staged"), "{err}");
    let err = run_err(&env, &["list", "nope"]);
    assert!(err.contains("No manual named 'nope'"), "{err}");
    run_ok(&env, &["schema", "--staged", "pets.get_user"]);
    let err = run_err(&env, &["schema", "pets.get_user"]);
    assert!(err.contains("not found"), "{err}");
    assert!(err.contains("utcp schema --staged pets.get_user"), "{err}");
    let err = run_err(&env, &["call", "pets.get_user", "{}"]);
    assert!(err.contains("utcp call --staged pets.get_user"), "{err}");
    let err = run_err(&env, &["call", "pets", "get_user"]);
    assert!(
        err.contains("Did you mean: utcp call --staged pets.get_user"),
        "{err}"
    );
    let err = run_err(&env, &["call", "pets", "get_us"]);
    assert!(!err.contains("Did you mean"), "{err}");
    assert!(
        err.contains("'pets' (staged) has no tool named 'get_us'"),
        "{err}"
    );
    assert!(err.contains("Similar tools: pets.get_user."), "{err}");
    assert!(err.contains("utcp list pets --staged"), "{err}");
    let err = run_err(&env, &["call", "pets.get_us", "{}"]);
    assert!(!err.contains("Add --staged"), "{err}");
    assert!(err.contains("has no tool named 'get_us'"), "{err}");
    let err = run_err(&env, &["call", "nope", "get_user"]);
    assert!(err.contains("No manual named 'nope'"), "{err}");
    let err = run_err(&env, &["call", "pest.get_user", "{}"]);
    assert!(err.contains("Similar manuals: pets."), "{err}");
    assert_eq!(err.matches("utcp list").count(), 1, "{err}");
    let err = run_err(&env, &["schema", "pest.get_user"]);
    assert!(err.contains("Similar manuals: pets."), "{err}");
    let err = run_err(&env, &["schema", "get_user"]);
    assert!(err.contains("Name a tool as <manual>.<tool>"), "{err}");
    assert_eq!(call_staged_auth(&env), "Bearer from-secrets");
    assert!(!installed_skill_description(&env).contains("pets"));
    let doctor = doctor_json(&env);
    assert_eq!(doctor["staged_count"], 1);
    assert_eq!(credential(&doctor, "pets")["staged"], true);
    assert_eq!(credential(&doctor, "pets")["source"], "secrets command");
    let text = run_ok(&env, &["doctor"]);
    assert!(text.contains("Staged Manuals    : 1"), "{text}");
    assert!(
        text.contains("(from secrets command; manual: pets (staged))"),
        "{text}"
    );
    let out = run_ok(&env, &["diff"]);
    assert!(out.contains("pets: new manual, 2 tools"), "{out}");
    assert!(out.contains("hint: pet store"), "{out}");
    assert!(
        out.contains("- See the tools: utcp list pets --staged"),
        "{out}"
    );
    assert!(
        out.contains("- Promote:       utcp promote pets\n"),
        "{out}"
    );
    assert!(
        out.contains("- Discard:       utcp discard pets\n"),
        "{out}"
    );
    // Promote, discard and refresh never act on everything by accident.
    for cmd in ["promote", "discard", "refresh"] {
        let err = run_err(&env, &[cmd]);
        assert!(err.contains("--all"), "{cmd}: {err}");
    }
    assert!(run_err(&env, &["promote", "pets", "--all"]).contains("cannot be used with"));

    // 2. The daemon loads staged manuals and asks the secrets command for their names too.
    run_ok(&env, &["daemon", "start"]);
    assert!(run_ok(&env, &["list", "--staged", "--tools"]).contains("pets.get_user"));
    assert!(!run_ok(&env, &["list", "--tools"]).contains("pets."));
    let out = run_ok(&env, &["search", "--staged", "user", "--json"]);
    assert!(out.contains("\"staged\": true"), "{out}");
    assert!(!run_ok(&env, &["search", "user"]).contains("pets."));
    let out = run_ok(&env, &["list"]);
    assert!(
        out.contains(
            "Staged (not active; see the tools with: utcp list <manual> --staged):\n* pets"
        ),
        "{out}"
    );
    let err = run_err(&env, &["list", "pets"]);
    assert!(err.contains("utcp list pets --staged"), "{err}");
    let err = run_err(&env, &["call", "pets.get_user", "{}"]);
    assert!(err.contains("utcp call --staged pets.get_user"), "{err}");
    let err = run_err(&env, &["call", "pets.get_us", "{}"]);
    assert!(err.contains("Similar tools: pets.get_user."), "{err}");
    // Through the daemon too: the error points to `utcp list` once, and names the guess once.
    let err = run_err(&env, &["call", "pest.get_user", "{}"]);
    assert!(err.contains("Similar manuals: pets."), "{err}");
    assert_eq!(err.matches("utcp list").count(), 1, "{err}");
    assert_eq!(call_staged_auth(&env), "Bearer from-secrets");
    let doctor = doctor_json(&env);
    assert_eq!(credential(&doctor, "pets")["is_set"], true);

    // 3. Promote: active in the daemon, search and skills; the staged copy is gone.
    let out = run_ok(&env, &["promote", "--all"]);
    assert!(out.contains("Promoted 'pets' (2 tools, new)"), "{out}");
    assert!(run_ok(&env, &["list", "--tools"]).contains("pets.get_user"));
    assert!(installed_skill_description(&env).contains("pets (pet store)"));
    assert!(run_ok(&env, &["list", "--staged", "--tools"]).contains("No matching tools"));
    // Without a manual, `list` names the manuals, with their tool counts and hints.
    let out = run_ok(&env, &["list"]);
    assert!(
        out.contains("Manuals (see the tools with: utcp list <manual>):")
            && out.contains("2 tools  pet store")
            && !out.contains("get_user")
            && !out.contains("Staged"),
        "{out}"
    );
    let rows: Value = serde_json::from_str(&run_ok(&env, &["list", "--json"])).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .contains(&json!({"name": "pets", "tools": 2, "hint": "pet store", "staged": false})),
        "{rows}"
    );
    assert_eq!(
        run_ok(&env, &["list", "--staged"]).trim(),
        "Nothing is staged."
    );
    let err = run_err(&env, &["call", "--staged", "pets.get_user", "{}"]);
    assert!(
        err.contains("Leave out --staged: utcp call pets.get_user"),
        "{err}"
    );
    assert!(!env
        .tmp
        .path()
        .join("cache/manuals/staged/pets.utcp.json")
        .exists());
    assert_eq!(doctor_json(&env)["staged_count"], 0);

    // 4. An update: diff names what changes; promote keeps the old version for rollback.
    let out = run_ok(&env, &["stage", "pets", &v2]);
    assert!(out.contains("Replaces  : the active manual"), "{out}");
    assert!(out.contains("Hint      : pet store"), "{out}");
    let out = run_ok(&env, &["diff", "pets"]);
    assert!(
        out.contains(
            "replaces the active manual, 2 tools (1 added, 1 removed, 1 changed, 0 unchanged)"
        ),
        "{out}"
    );
    for line in [
        "  + new_tool",
        "  - old_tool",
        "  ~ get_user: description\n",
    ] {
        assert!(out.contains(line), "{line}: {out}");
    }
    let diff: Value = serde_json::from_str(&run_ok(&env, &["diff", "--json"])).unwrap();
    assert_eq!(diff[0]["replaces"], "active");
    assert_eq!(
        diff[0]["changed"],
        json!([{"tool": "get_user", "what": ["description"]}])
    );
    let out = run_ok(&env, &["promote", "pets"]);
    assert!(out.contains("undo with: utcp rollback pets"), "{out}");
    let list = run_ok(&env, &["list", "--tools"]);
    assert!(
        list.contains("pets.new_tool") && !list.contains("pets.old_tool"),
        "{list}"
    );
    assert!(installed_skill_description(&env).contains("pets (pet store)"));

    // 5. Rollback swaps back; a second rollback undoes the first.
    let out = run_ok(&env, &["rollback", "pets"]);
    assert!(out.contains("again to undo"), "{out}");
    assert!(run_ok(&env, &["list", "--tools"]).contains("pets.old_tool"));
    run_ok(&env, &["rollback", "pets"]);
    assert!(run_ok(&env, &["list", "--tools"]).contains("pets.new_tool"));
    assert!(run_err(&env, &["rollback", "nope"]).contains("No previous version of 'nope'"));

    // 6. Discard, and errors for names that are not staged.
    run_ok(&env, &["stage", "pets", &v1]);
    assert!(run_ok(&env, &["discard", "pets"]).contains("Discarded the staged copy of: pets"));
    assert!(run_ok(&env, &["discard", "--all"]).contains("Nothing is staged."));
    assert!(run_ok(&env, &["promote", "--all"]).contains("Nothing is staged."));
    assert!(run_err(&env, &["promote", "nope"]).contains("'nope' is not staged"));
    assert!(run_ok(&env, &["list", "--tools"]).contains("pets.new_tool"));

    // 6b. Several names and prefix patterns; the diff footer prints the ones to type.
    for name in ["shop_cart", "shop_orders", "zoo"] {
        run_ok(&env, &["stage", name, &v1]);
    }
    let out = run_ok(&env, &["diff"]);
    assert!(
        out.contains("- Promote:       utcp promote 'shop_*' zoo\n"),
        "{out}"
    );
    let out = run_ok(&env, &["diff", "shop_*"]);
    assert!(
        out.contains("shop_cart: new manual") && !out.contains("zoo:"),
        "{out}"
    );
    let out = run_ok(&env, &["diff", "shop_cart", "zoo"]);
    assert!(
        out.contains("- Promote:       utcp promote shop_cart zoo\n"),
        "{out}"
    );
    let err = run_err(&env, &["promote", "shop_*", "nope_*"]);
    assert!(
        err.contains("No staged manual matches 'nope_*' (staged: shop_cart, shop_orders, zoo)"),
        "{err}"
    );
    assert!(run_err(&env, &["promote", "s*p_*"]).contains("only a trailing *"));
    let out = run_ok(&env, &["promote", "shop_*", "shop_cart"]);
    assert!(
        out.contains("Promoted 'shop_cart'") && out.contains("Promoted 'shop_orders'"),
        "{out}"
    );
    assert_eq!(out.matches("Promoted 'shop_cart'").count(), 1, "{out}");
    assert!(run_ok(&env, &["list", "--staged"]).contains("zoo"));
    let out = run_ok(&env, &["refresh", "shop_*", "pets"]);
    assert_eq!(out.matches("Refreshed '").count(), 3, "{out}");
    let err = run_err(&env, &["refresh", "zoo"]);
    assert!(
        err.contains("'zoo' is not registered (registered: pets, shop_cart"),
        "{err}"
    );
    assert!(run_ok(&env, &["discard", "zoo"]).contains("Discarded the staged copy of: zoo"));

    // 7. Purge deletes the previous version and any staged copy too.
    run_ok(&env, &["stage", "pets", &v1]);
    run_ok(&env, &["deregister", "pets", "--purge-cache"]);
    let cfg = read_config(&env);
    assert!(
        cfg.get("previous").is_none() && cfg.get("staged").is_none(),
        "{cfg}"
    );
    for dir in ["previous", "staged"] {
        let file = env
            .tmp
            .path()
            .join(format!("cache/manuals/{dir}/pets.utcp.json"));
        assert!(!file.exists(), "{}", file.display());
    }
}

/// Registering in another setup (another config folder, same home) leaves the skills of the setup that wrote them
/// alone, until `sync-ides` in the other setup takes them over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skills_are_refreshed_only_by_the_setup_that_wrote_them() {
    let base_url = spawn_mock_api().await;
    let spec_url = format!("{base_url}/openapi.json");
    let env = IsolatedEnv::new();
    std::fs::create_dir_all(env.tmp.path().join("home")).unwrap();
    run_ok(&env, &["sync-ides", "--global"]);
    run_ok(&env, &["register", "mine", &spec_url]);
    assert!(installed_skill_description(&env).contains("mine"));

    let other = |args: &[&str]| {
        let root = env.tmp.path();
        let out = env
            .cmd()
            .env("UTCP_CONFIG_DIR", root.join("other-config"))
            .env("UTCP_CACHE_DIR", root.join("other-cache"))
            .env("UTCP_STATE_DIR", root.join("other-state"))
            .env_remove("UTCP_SOCKET_PATH")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    other(&["register", "theirs", &spec_url]);
    let description = installed_skill_description(&env);
    assert!(
        description.contains("mine") && !description.contains("theirs"),
        "{description}"
    );

    // Skills written before the owner file existed belong to the default setup only.
    let owner = env
        .tmp
        .path()
        .join("home/.agents/skills/utcp-tools/.utcp-registry");
    std::fs::remove_file(&owner).unwrap();
    other(&["deregister", "theirs"]);
    run_ok(&env, &["register", "mine2", &spec_url]);
    assert!(!installed_skill_description(&env).contains("mine2"));

    // `sync-ides` takes the folders over explicitly.
    other(&["register", "theirs"]);
    other(&["sync-ides", "--global"]);
    let description = installed_skill_description(&env);
    assert!(
        description.contains("theirs") && !description.contains("mine"),
        "{description}"
    );
    run_ok(&env, &["deregister", "mine"]);
    assert!(installed_skill_description(&env).contains("theirs"));
}

/// A setup with its own config or cache folder gets its own daemon endpoint unless `UTCP_SOCKET_PATH` says otherwise,
/// so it never reaches the default setup's daemon.
#[test]
fn a_separate_setup_gets_its_own_daemon_endpoint() {
    let endpoint = |env: &IsolatedEnv| {
        let out = env
            .cmd()
            .env_remove("UTCP_SOCKET_PATH")
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["doctor"]["socket_endpoint"].as_str().unwrap().to_string()
    };
    let (a, b) = (IsolatedEnv::new(), IsolatedEnv::new());
    let (ea, eb) = (endpoint(&a), endpoint(&b));
    assert_ne!(ea, eb);
    #[cfg(unix)]
    assert_eq!(
        ea,
        a.tmp
            .path()
            .join("cache")
            .join("utcp.sock")
            .display()
            .to_string()
    );
    #[cfg(windows)]
    assert!(ea.starts_with(r"\\.\pipe\utcp-"), "{ea}");
}

/// A setup with its own config file but no `UTCP_CACHE_DIR` gets its own cache folder (and so its own search index
/// and daemon endpoint), keyed by the config file, instead of sharing the default setup's.
#[cfg(target_os = "linux")]
#[test]
fn an_own_config_without_a_cache_dir_gets_its_own_cache() {
    let env = IsolatedEnv::new();
    let xdg_cache = env.tmp.path().join("xdg-cache");
    let doctor = |config: &str| {
        let out = env
            .cmd()
            .env_remove("UTCP_CACHE_DIR")
            .env_remove("UTCP_SOCKET_PATH")
            .env("XDG_CACHE_HOME", &xdg_cache)
            .arg("--config")
            .arg(env.tmp.path().join(config))
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let cache = v["doctor"]["cache_dir"].as_str().unwrap().to_string();
        let endpoint = v["doctor"]["socket_endpoint"].as_str().unwrap().to_string();
        (cache, endpoint)
    };
    let (cache_a, endpoint_a) = doctor("a.json");
    let (cache_b, _) = doctor("b.json");
    let setups = xdg_cache.join("utcp").join("setups");
    assert!(
        std::path::Path::new(&cache_a).parent() == Some(setups.as_path()),
        "{cache_a}"
    );
    assert_ne!(cache_a, cache_b);
    assert_eq!(
        cache_a,
        doctor("a.json").0,
        "the same config keeps its cache"
    );
    assert_eq!(
        endpoint_a,
        std::path::Path::new(&cache_a)
            .join("utcp.sock")
            .display()
            .to_string()
    );
}

/// Bytes 0..=255 over and over, `len` of them.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

const ZIP_LEN: usize = 300 * 1024;

/// A server with a file download, a text answer, an answer too large to print, and an upload.
async fn spawn_file_api() -> String {
    use axum::body::Bytes;
    let zip = || async {
        (
            [
                // Parameters as Azure DevOps sends them; the answer reports only the type.
                ("content-type", "application/zip; api-version=7.1"),
                ("content-disposition", "attachment; filename=\"build.zip\""),
            ],
            pattern(ZIP_LEN),
        )
    };
    let log = || async { ([("content-type", "text/plain")], "line1\nline2") };
    let big = || async { Json(json!({"blob": "x".repeat(11 * 1024 * 1024), "count": 1})) };
    let upload = |headers: HeaderMap, body: Bytes| async move {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        Json(json!({
            "len": body.len(),
            "sum": body.iter().map(|b| *b as u64).sum::<u64>(),
            "first": body.first(),
            "content_type": header("content-type"),
            "content_length": header("content-length"),
        }))
    };
    let app = Router::new()
        .route("/zip", get(zip))
        .route("/log", get(log))
        .route("/big", get(big))
        .route("/upload", post(upload));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

fn file_manual(env: &IsolatedEnv, base: &str) -> String {
    let tool = |name: &str, method: &str, path: &str, extra: Value| {
        let mut template = json!({
            "call_template_type": "http",
            "http_method": method,
            "url": format!("{base}{path}"),
        });
        for (k, v) in extra.as_object().unwrap() {
            template[k] = v.clone();
        }
        json!({"name": name, "description": name, "inputs": {"type": "object"},
               "outputs": {"type": "object"}, "tool_call_template": template})
    };
    let manual = json!({
        "manual_version": "1.0.0",
        "utcp_version": "1.0.0",
        "info": {"title": "files", "version": "1"},
        "tools": [
            tool("get_zip", "GET", "/zip", json!({})),
            tool("get_log", "GET", "/log", json!({})),
            tool("get_log_file", "GET", "/log", json!({"response_type": "file"})),
            tool("get_big", "GET", "/big", json!({})),
            tool("upload", "POST", "/upload", json!({
                "body_field": "body",
                "body_type": "file",
                "headers": {"Content-Type": "application/octet-stream"},
            })),
        ],
    });
    let path = env.tmp.path().join("files.utcp.json");
    std::fs::write(&path, manual.to_string()).unwrap();
    path.display().to_string()
}

/// Downloads (default folder, `--out` file, folder and stdout), text answers, answers too large to print,
/// uploads (whole, a part, a relative path), both limits and batch `out`: through the daemon and in direct mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_are_uploaded_and_downloaded_by_the_calling_process() {
    let env = IsolatedEnv::new();
    let base = spawn_file_api().await;
    let root = env.tmp.path().to_path_buf();
    let dl = root.join("dl");
    std::fs::create_dir_all(root.join("config")).unwrap();
    let settings = |extra: &str| {
        std::fs::write(
            root.join("config/settings.toml"),
            format!(
                "download_dir = {}\nmax_upload_mb = 1\n{extra}",
                toml_string(&dl.display().to_string())
            ),
        )
        .unwrap();
    };
    settings("");
    run_ok(&env, &["register", "files", &file_manual(&env, &base)]);
    std::fs::write(root.join("up.bin"), pattern(200 * 1024)).unwrap();
    std::fs::write(root.join("too_big.bin"), pattern(2 * 1024 * 1024)).unwrap();

    let call = |mode: &[&str], args: &[&str]| -> Value {
        let mut all: Vec<&str> = mode.to_vec();
        all.push("call");
        all.extend_from_slice(args);
        let out = env.cmd().current_dir(&root).args(&all).output().unwrap();
        assert!(
            out.status.success(),
            "utcp {all:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let call_err = |mode: &[&str], args: &[&str]| -> String {
        let mut all: Vec<&str> = mode.to_vec();
        all.push("call");
        all.extend_from_slice(args);
        let out = env.cmd().current_dir(&root).args(&all).output().unwrap();
        assert!(!out.status.success(), "utcp {all:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    for (label, mode) in [("daemon", &[][..]), ("direct", &["--direct"][..])] {
        if label == "daemon" {
            run_ok(&env, &["daemon", "start"]);
        } else {
            run_ok(&env, &["daemon", "stop"]);
        }
        let _ = std::fs::remove_dir_all(&dl);

        // A file answer goes to the downloads folder under the server's name, never overwriting.
        let r = call(mode, &["files.get_zip"]);
        assert_eq!(r["type"], "file", "{label}: {r}");
        assert_eq!(r["mime_type"], "application/zip");
        assert_eq!(r["bytes"], ZIP_LEN);
        assert_eq!(r["filename"], "build.zip");
        let saved = std::path::PathBuf::from(r["path"].as_str().unwrap());
        assert_eq!(saved, dl.join("build.zip"), "{label}");
        assert_eq!(std::fs::read(&saved).unwrap(), pattern(ZIP_LEN));
        let r = call(mode, &["files.get_zip"]);
        assert!(
            r["path"].as_str().unwrap().ends_with("build (1).zip"),
            "{label}: {r}"
        );

        // --out overwrites a file, keeps the server's name in a folder, and `-` is stdout.
        let out_file = root.join("x.zip");
        std::fs::write(&out_file, "old").unwrap();
        call(mode, &["files.get_zip", "--out", "x.zip"]);
        assert_eq!(
            std::fs::read(&out_file).unwrap(),
            pattern(ZIP_LEN),
            "{label}"
        );
        let folder = root.join("outdir");
        std::fs::create_dir_all(&folder).unwrap();
        call(
            mode,
            &["files.get_zip", "--out", &folder.display().to_string()],
        );
        assert!(folder.join("build.zip").is_file(), "{label}");
        let mut args: Vec<&str> = mode.to_vec();
        args.extend(["call", "files.get_zip", "--out", "-"]);
        let raw = env.cmd().args(&args).output().unwrap();
        assert!(raw.status.success(), "{label}");
        assert_eq!(
            raw.stdout,
            pattern(ZIP_LEN),
            "{label}: --out - writes only the bytes"
        );
        // A reader that stops early (`| head -c 100`) ends the download quietly.
        let mut child = env
            .cmd()
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let closed = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&closed.stderr);
        assert!(closed.status.success(), "{label}: {stderr}");
        assert!(stderr.is_empty(), "{label}: {stderr}");

        // Text stays a JSON string unless the template marks the answer as a file.
        assert_eq!(call(mode, &["files.get_log"]), json!("line1\nline2"));
        let r = call(mode, &["files.get_log_file"]);
        assert!(
            r["path"].as_str().unwrap().ends_with("get_log_file.txt"),
            "{label}: {r}"
        );
        // A JSON answer with --out goes to that file.
        call(mode, &["files.get_log", "--out", "log.json"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(root.join("log.json")).unwrap())
                .unwrap(),
            json!("line1\nline2")
        );

        // An answer too large to print is saved in utcp's own folder, with a note.
        let r = call(mode, &["files.get_big"]);
        let big = std::path::PathBuf::from(r["path"].as_str().unwrap());
        assert!(
            big.starts_with(root.join("cache").join("downloads")),
            "{label}: {r}"
        );
        assert!(r["note"].as_str().unwrap().contains("after a day"));
        let parsed: Value = serde_json::from_slice(&std::fs::read(&big).unwrap()).unwrap();
        assert_eq!(parsed["count"], 1);
        // --fields first: what is left is small enough to print.
        assert_eq!(
            call(mode, &["files.get_big", "--fields", "count"]),
            json!({"count": 1})
        );

        // Uploads: the calling process reads the file (a relative path is relative to it).
        let r = call(mode, &["files.upload", r#"{"body": {"file": "up.bin"}}"#]);
        let sent = pattern(200 * 1024);
        assert_eq!(r["len"], sent.len(), "{label}: {r}");
        assert_eq!(r["sum"], sent.iter().map(|b| *b as u64).sum::<u64>());
        assert_eq!(r["content_type"], "application/octet-stream");
        assert_eq!(r["content_length"], sent.len().to_string());
        let r = call(
            mode,
            &[
                "files.upload",
                r#"{"body": {"file": "up.bin", "offset": 10, "length": 1000}}"#,
            ],
        );
        assert_eq!(
            (r["len"].clone(), r["first"].clone()),
            (json!(1000), json!(10))
        );
        let err = call_err(
            mode,
            &["files.upload", r#"{"body": {"file": "too_big.bin"}}"#],
        );
        assert!(err.contains("only because of this limit"), "{label}: {err}");
        assert!(err.contains("max_upload_mb"), "{label}: {err}");
        let err = call_err(
            mode,
            &["files.upload", r#"{"body": {"file": "missing.bin"}}"#],
        );
        assert!(err.contains("cannot upload"), "{label}: {err}");

        // The download limit stops a file answer and keeps nothing; JSON answers are not limited.
        settings("max_download_mb = 0\n");
        let err = call_err(mode, &["files.get_zip", "--out", "limited.zip"]);
        assert!(err.contains("downloads are turned off"), "{label}: {err}");
        assert!(!root.join("limited.zip").exists());
        assert!(!root.join("limited.zip.part").exists());
        assert_eq!(call(mode, &["files.get_log"]), json!("line1\nline2"));
        settings("");

        // Batch: each item may have its own `out`.
        let batch = json!([
            {"id": "a", "tool": "files.get_zip", "out": root.join("b.zip").display().to_string()},
            {"id": "b", "tool": "files.get_log"},
            {"id": "c", "tool": "files.upload", "args": {"body": {"file": "up.bin"}}},
        ]);
        std::fs::write(root.join("batch.json"), batch.to_string()).unwrap();
        let mut args: Vec<&str> = mode.to_vec();
        args.extend(["batch", "--file", "batch.json"]);
        let out = env.cmd().current_dir(&root).args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let results: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(results[0]["ok"], true, "{label}: {results}");
        assert_eq!(std::fs::read(root.join("b.zip")).unwrap(), pattern(ZIP_LEN));
        assert_eq!(results[1]["result"], json!("line1\nline2"));
        assert_eq!(
            results[2]["result"]["len"],
            200 * 1024,
            "{label}: {results}"
        );
        if label == "daemon" {
            let status: Value =
                serde_json::from_str(&run_ok(&env, &["daemon", "status", "--json"])).unwrap();
            assert!(
                status["requests_served"].as_u64().unwrap() >= 15,
                "the calls went through the daemon: {status}"
            );
        }
    }
}

/// `--dry-run` shows where each argument goes: path, header, query or body, including a JSON body
/// on DELETE and a raw text body. Nothing is sent (the URL has no server).
#[test]
fn dry_run_shows_the_query_and_the_body_it_would_send() {
    let env = IsolatedEnv::new();
    let base = "http://127.0.0.1:9";
    let tool = |name: &str, method: &str, path: &str, extra: Value| {
        let mut template = json!({
            "call_template_type": "http",
            "http_method": method,
            "url": format!("{base}{path}"),
        });
        for (k, v) in extra.as_object().unwrap() {
            template[k] = v.clone();
        }
        json!({"name": name, "description": name, "inputs": {"type": "object"},
               "outputs": {"type": "object"}, "tool_call_template": template})
    };
    let manual = json!({
        "manual_version": "1.0.0",
        "utcp_version": "1.0.0",
        "info": {"title": "bodies", "version": "1"},
        "tools": [
            tool("delete_file", "DELETE", "/contents/{path}", json!({
                "body_field": "body", "header_fields": ["X-Trace"],
            })),
            tool("delete_plain", "DELETE", "/items/{id}", json!({})),
            tool("render_raw", "POST", "/markdown/raw", json!({
                "body_field": "text", "body_type": "text",
                "headers": {"Content-Type": "text/x-markdown"},
            })),
        ],
    });
    let path = env.tmp.path().join("bodies.utcp.json");
    std::fs::write(&path, manual.to_string()).unwrap();
    run_ok(&env, &["register", "bodies", &path.display().to_string()]);

    for (label, mode) in [("direct", &["--direct"][..]), ("daemon", &[][..])] {
        if label == "daemon" {
            run_ok(&env, &["daemon", "start"]);
        }
        let dry = |tool: &str, args: Value| -> Value {
            let mut all: Vec<&str> = mode.to_vec();
            let args = args.to_string();
            all.extend_from_slice(&["call", tool, &args, "--dry-run"]);
            serde_json::from_str(&run_ok(&env, &all)).unwrap()
        };
        let v = dry(
            "bodies.delete_file",
            json!({"path": "a b.md", "X-Trace": "t1", "branch": "main",
                   "body": {"message": "m", "sha": "s"}}),
        );
        assert_eq!(v["method"], "DELETE", "{label}");
        assert_eq!(v["url"], format!("{base}/contents/a%20b.md"));
        assert_eq!(v["headers"]["X-Trace"], "t1");
        assert_eq!(v["query"], json!({"branch": "main"}));
        assert_eq!(v["body"], json!({"message": "m", "sha": "s"}));

        let v = dry("bodies.delete_plain", json!({"id": 3, "force": true}));
        assert_eq!(v["query"], json!({"force": "true"}), "{label}");
        assert!(v.get("body").is_none(), "{v}");

        let v = dry("bodies.render_raw", json!({"text": "# Hi"}));
        assert_eq!(v["body"], "# Hi", "{label}");
        assert_eq!(v["body_type"], "text");
        assert_eq!(v["headers"]["Content-Type"], "text/x-markdown");
    }
    run_ok(&env, &["daemon", "stop"]);
}

/// Lists that page the way GitHub (`Link`) and Azure DevOps (`x-ms-continuationtoken`, as a query
/// argument or a header) do. The last page carries no paging header.
async fn spawn_paged_api() -> String {
    use axum::response::IntoResponse;
    let items = |headers: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("127.0.0.1")
            .to_string();
        match q.get("page").map(String::as_str) {
            None | Some("1") => {
                let per_page = q.get("per_page").cloned().unwrap_or_default();
                let link = format!(
                    "<http://{host}/items?per_page={per_page}&page=2>; rel=\"next\", \
                     <http://{host}/items?per_page={per_page}&page=2>; rel=\"last\""
                );
                (
                    [("link", link)],
                    Json(json!([{"id": 1, "name": "a"}, {"id": 2, "name": "b"}])),
                )
                    .into_response()
            }
            _ => Json(json!([{"id": 3, "name": "c"}])).into_response(),
        }
    };
    let projects = |Query(q): Query<HashMap<String, String>>| async move {
        match q.get("continuationToken").map(String::as_str) {
            None => (
                [("x-ms-continuationtoken", "tok2")],
                Json(json!({"count": 1, "value": [{"name": "p1"}]})),
            )
                .into_response(),
            Some(_) => Json(json!({"count": 1, "value": [{"name": "p2"}]})).into_response(),
        }
    };
    let builds = |headers: HeaderMap| async move {
        match headers.get("x-ms-continuationtoken") {
            None => (
                [("x-ms-continuationtoken", "b2")],
                Json(json!({"value": [1]})),
            )
                .into_response(),
            Some(_) => Json(json!({"value": [2]})).into_response(),
        }
    };
    let app = Router::new()
        .route("/items", get(items))
        .route("/projects", get(projects))
        .route("/builds", get(builds));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

/// A page of a longer list prints as `{result, next_page}`; calling again with `next_page` added
/// gets the next page, and the last page is plain JSON. `--fields` and `--out` apply to `result`;
/// batch items carry `next_page` beside `result`. Through the daemon and in direct mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paged_answers_name_the_arguments_for_the_next_page() {
    let env = IsolatedEnv::new();
    let base = spawn_paged_api().await;
    let root = env.tmp.path().to_path_buf();
    let tool = |name: &str, path: &str, extra: Value| {
        let mut template = json!({
            "call_template_type": "http",
            "http_method": "GET",
            "url": format!("{base}{path}"),
        });
        for (k, v) in extra.as_object().unwrap() {
            template[k] = v.clone();
        }
        json!({"name": name, "description": name, "inputs": {"type": "object"},
               "outputs": {"type": "object"}, "tool_call_template": template})
    };
    let manual = json!({
        "manual_version": "1.0.0",
        "utcp_version": "1.0.0",
        "info": {"title": "paged", "version": "1"},
        "tools": [
            tool("list_items", "/items", json!({})),
            tool("list_projects", "/projects", json!({})),
            tool("list_builds", "/builds", json!({"header_fields": ["x-ms-continuationtoken"]})),
        ],
    });
    let path = root.join("paged.utcp.json");
    std::fs::write(&path, manual.to_string()).unwrap();
    run_ok(&env, &["register", "paged", &path.display().to_string()]);

    for (label, mode) in [("daemon", &[][..]), ("direct", &["--direct"][..])] {
        if label == "daemon" {
            run_ok(&env, &["daemon", "start"]);
        } else {
            run_ok(&env, &["daemon", "stop"]);
        }
        let call = |args: &[&str]| -> Value {
            let mut all: Vec<&str> = mode.to_vec();
            all.push("call");
            all.extend_from_slice(args);
            let out = env.cmd().current_dir(&root).args(&all).output().unwrap();
            assert!(
                out.status.success(),
                "utcp {all:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        };
        let with_next = |args: Value, next_page: &Value| -> String {
            let mut args = args;
            for (k, v) in next_page.as_object().unwrap() {
                args[k] = v.clone();
            }
            args.to_string()
        };

        // Link: only the arguments that change (`page`), as strings.
        let first_args = json!({"per_page": 2});
        let r = call(&["paged.list_items", &first_args.to_string()]);
        assert_eq!(r["next_page"], json!({"page": "2"}), "{label}: {r}");
        assert_eq!(r["result"].as_array().unwrap().len(), 2, "{label}: {r}");
        let r = call(&[
            "paged.list_items",
            &with_next(first_args.clone(), &r["next_page"]),
        ]);
        assert_eq!(
            r,
            json!([{"id": 3, "name": "c"}]),
            "{label}: the last page is plain"
        );

        // Azure DevOps: a query argument, or the header field the template declares.
        let r = call(&["paged.list_projects"]);
        assert_eq!(
            r["next_page"],
            json!({"continuationToken": "tok2"}),
            "{label}: {r}"
        );
        assert_eq!(r["result"]["value"][0]["name"], "p1");
        let r = call(&[
            "paged.list_projects",
            &with_next(json!({}), &r["next_page"]),
        ]);
        assert_eq!(r["value"][0]["name"], "p2", "{label}: {r}");
        assert!(r.get("next_page").is_none());
        let r = call(&["paged.list_builds"]);
        assert_eq!(
            r["next_page"],
            json!({"x-ms-continuationtoken": "b2"}),
            "{label}: {r}"
        );
        let r = call(&["paged.list_builds", &with_next(json!({}), &r["next_page"])]);
        assert_eq!(r, json!({"value": [2]}), "{label}");

        // --fields projects the answer, not the page around it.
        let r = call(&["paged.list_projects", "--fields", "count"]);
        assert_eq!(
            r,
            json!({"result": {"count": 1}, "next_page": {"continuationToken": "tok2"}}),
            "{label}"
        );

        // --out saves the answer; the next page's arguments are still printed.
        let _ = std::fs::remove_file(root.join("page.json"));
        let r = call(&["paged.list_projects", "--out", "page.json"]);
        assert_eq!(
            r["next_page"],
            json!({"continuationToken": "tok2"}),
            "{label}: {r}"
        );
        assert_eq!(r["result"]["type"], "file", "{label}: {r}");
        let saved: Value =
            serde_json::from_slice(&std::fs::read(root.join("page.json")).unwrap()).unwrap();
        assert_eq!(saved["value"][0]["name"], "p1", "{label}");

        // Batch: `next_page` beside the item's `result`, only on items that have one.
        let batch = json!([
            {"id": "a", "tool": "paged.list_items", "args": {"per_page": 2}},
            {"id": "b", "tool": "paged.list_items", "args": {"per_page": 2, "page": "2"}},
            {"id": "c", "tool": "paged.list_projects", "fields": ["count"]},
        ]);
        std::fs::write(root.join("batch.json"), batch.to_string()).unwrap();
        let mut args: Vec<&str> = mode.to_vec();
        args.extend(["batch", "--file", "batch.json"]);
        let out = env.cmd().current_dir(&root).args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let results: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            results[0]["next_page"],
            json!({"page": "2"}),
            "{label}: {results}"
        );
        assert_eq!(results[0]["result"].as_array().unwrap().len(), 2);
        assert!(results[1].get("next_page").is_none(), "{label}: {results}");
        assert_eq!(results[1]["result"][0]["id"], 3);
        assert_eq!(
            results[2]["result"],
            json!({"count": 1}),
            "{label}: {results}"
        );
        assert_eq!(
            results[2]["next_page"],
            json!({"continuationToken": "tok2"})
        );
    }
}

/// Answers shaped like an Azure DevOps build timeline (a top-level `records` list beside other
/// fields), an answer with two lists, and a single object.
async fn spawn_where_api() -> String {
    let timeline = || async {
        Json(json!({
            "id": "t1",
            "records": [
                {"name": "a", "type": "Task", "result": "failed", "issues": [{"type": "error"}]},
                {"name": "b", "type": "Job", "result": "canceled", "issues": []},
                {"name": "c", "type": "Task", "result": "succeeded"},
                {"name": "d", "type": "Task", "result": null},
            ],
        }))
    };
    let app = Router::new()
        .route("/timeline", get(timeline))
        .route(
            "/two",
            get(|| async { Json(json!({"a": [{"x": 1}], "b": [{"x": 2}]})) }),
        )
        .route(
            "/lines",
            get(|| async { Json(json!({"count": 2, "value": ["ok", "error: x"]})) }),
        )
        .route(
            "/one",
            get(|| async { Json(json!({"id": 1, "name": "x"})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

fn get_tool(name: &str, url: String) -> Value {
    json!({"name": name, "description": name, "inputs": {"type": "object"},
           "outputs": {"type": "object"},
           "tool_call_template": {"call_template_type": "http", "http_method": "GET", "url": url}})
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect()
}

/// `--where` keeps the list items that match, before `--fields`, on `call` and `batch`, through the
/// daemon and in direct mode. A page keeps its `next_page` even when nothing on it matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn where_keeps_the_matching_list_items() {
    let env = IsolatedEnv::new();
    let base = spawn_where_api().await;
    let paged = spawn_paged_api().await;
    let root = env.tmp.path().to_path_buf();
    let manual = json!({
        "manual_version": "1.0.0",
        "utcp_version": "1.0.0",
        "info": {"title": "w", "version": "1"},
        "tools": [
            get_tool("timeline", format!("{base}/timeline")),
            get_tool("two", format!("{base}/two")),
            get_tool("one", format!("{base}/one")),
            get_tool("lines", format!("{base}/lines")),
            get_tool("list_items", format!("{paged}/items")),
        ],
    });
    let path = root.join("w.utcp.json");
    std::fs::write(&path, manual.to_string()).unwrap();
    run_ok(&env, &["register", "w", &path.display().to_string()]);

    for (label, mode) in [("daemon", &[][..]), ("direct", &["--direct"][..])] {
        if label == "daemon" {
            run_ok(&env, &["daemon", "start"]);
            let status: Value =
                serde_json::from_str(&run_ok(&env, &["daemon", "status", "--json"])).unwrap();
            assert_eq!(status["version"], env!("CARGO_PKG_VERSION"), "{status}");
            assert_eq!(
                status["features"],
                json!(["where", "search_staged"]),
                "{status}"
            );
            let text = run_ok(&env, &["daemon", "status"]);
            assert!(
                text.contains(&format!("Version      : {}", env!("CARGO_PKG_VERSION"))),
                "{text}"
            );
            assert!(!text.contains("WARNING"), "{text}");
        } else {
            run_ok(&env, &["daemon", "stop"]);
        }
        let with_mode = |args: &[&str]| -> Vec<String> {
            mode.iter()
                .chain(args.iter())
                .map(|s| s.to_string())
                .collect()
        };
        let call = |args: &[&str]| -> Value {
            let all = with_mode(&[&["call"][..], args].concat());
            serde_json::from_str(&run_ok(
                &env,
                &all.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
            .unwrap()
        };
        let call_err = |args: &[&str]| -> String {
            let all = with_mode(&[&["call"][..], args].concat());
            run_err(&env, &all.iter().map(String::as_str).collect::<Vec<_>>())
        };

        // The path names the top-level list (`records.`), or the answer's list is found for it;
        // the rest of the answer stays.
        for cond in ["records.result=failed|canceled", "result=failed| canceled"] {
            let r = call(&["w.timeline", "--where", cond]);
            assert_eq!(names(&r["records"]), ["a", "b"], "{label} {cond}: {r}");
            assert_eq!(r["id"], "t1", "{label}: {r}");
        }
        // Every condition must hold; `!=` keeps items without the value (a null result too).
        let r = call(&[
            "w.timeline",
            "--where",
            "result!=succeeded",
            "--where",
            "type=Task",
        ]);
        assert_eq!(names(&r["records"]), ["a", "d"], "{label}: {r}");
        let r = call(&["w.timeline", "--where", "result=null"]);
        assert_eq!(names(&r["records"]), ["d"], "{label}: {r}");
        // Through an array: any element may match.
        let r = call(&["w.timeline", "--where", "records.issues.type=error"]);
        assert_eq!(names(&r["records"]), ["a"], "{label}: {r}");
        // Contains and starts with, ignoring case; `!` before the operator keeps the others.
        let r = call(&["w.timeline", "--where", "result~=AIL|anc"]);
        assert_eq!(names(&r["records"]), ["a", "b"], "{label}: {r}");
        let r = call(&["w.timeline", "--where", "type!^=t"]);
        assert_eq!(names(&r["records"]), ["b"], "{label}: {r}");
        // A path part that matches no key exactly matches the key differing only in case.
        let r = call(&["w.timeline", "--where", "Records.Issues.Type=error"]);
        assert_eq!(names(&r["records"]), ["a"], "{label}: {r}");
        // Filtered first, then projected.
        let r = call(&[
            "w.timeline",
            "--where",
            "type=Job",
            "--fields",
            "records.name,records.result",
        ]);
        assert_eq!(
            r,
            json!({"records": [{"name": "b", "result": "canceled"}]}),
            "{label}"
        );

        // A page: the filter applies to `result`; `next_page` stays, even on an empty page.
        let r = call(&["w.list_items", r#"{"per_page": 2}"#, "--where", "name=b"]);
        assert_eq!(
            r,
            json!({"result": [{"id": 2, "name": "b"}], "next_page": {"page": "2"}}),
            "{label}"
        );
        // Keeping none of the items: stdout is the answer as it is, the note (with the values
        // the field has) goes to stderr.
        let all = with_mode(&[
            "call",
            "w.list_items",
            r#"{"per_page": 2}"#,
            "--where",
            "name=zz",
        ]);
        let out = env
            .cmd()
            .args(all.iter().map(String::as_str))
            .output()
            .unwrap();
        assert!(out.status.success(), "{label}");
        let r: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            r,
            json!({"result": [], "next_page": {"page": "2"}}),
            "{label}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(r#"note: --where 'name=zz' kept none of the 2 items in the answer (name here: "a", "b")"#),
            "{label}: {stderr}"
        );

        // A condition that can't apply to the answer (known only once it came back, so the call
        // was sent, perhaps a write): the answer, unfiltered, exit 0, and a note on stderr.
        let call_noted = |args: &[&str]| -> (Value, String) {
            let all = with_mode(&[&["call"][..], args].concat());
            let out = env
                .cmd()
                .args(all.iter().map(String::as_str))
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(out.status.success(), "{label} {args:?}: {stderr}");
            (serde_json::from_slice(&out.stdout).unwrap(), stderr)
        };
        let not_applied = "note: the call succeeded; --where was not applied";
        let (r, e) = call_noted(&["w.two", "--where", "x=1"]);
        assert!(r["a"].is_array() && r["b"].is_array(), "{label}: {r}");
        assert!(
            e.contains(not_applied) && e.contains("several lists (a, b)") && e.contains("a.x"),
            "{label}: {e}"
        );
        let (r, e) = call_noted(&["w.one", "--where", "name=x"]);
        assert!(r.is_object(), "{label}: {r}");
        assert!(
            e.contains(not_applied) && e.contains("one object, not a list"),
            "{label}: {e}"
        );
        let (r, e) = call_noted(&["w.timeline", "--where", "id=t1", "--fields", "id"]);
        assert_eq!(r, json!({"id": "t1"}), "{label}: --fields still applies");
        assert!(
            e.contains(not_applied)
                && e.contains("id is a field of the answer itself")
                && e.contains("records"),
            "{label}: {e}"
        );
        let (_, e) = call_noted(&["w.lines", "--where", "value~=error"]);
        assert!(e.contains("value is a list in the answer"), "{label}: {e}");
        let (_, e) = call_noted(&["w.lines", "--where", "text~=error"]);
        assert!(e.contains("list of plain values"), "{label}: {e}");
        // Errors in the condition itself stop the call before anything is sent.
        let e = call_err(&["w.timeline", "--where", "result~=fail|"]);
        assert!(e.contains("empty value"), "{label}: {e}");
        let e = call_err(&["w.timeline", "--where", "result"]);
        assert!(e.contains("has no operator"), "{label}: {e}");
        let e = call_err(&["w.timeline", "--where", "=failed"]);
        assert!(e.contains("names no field"), "{label}: {e}");

        // Batch: an item's own "where", else the default `--where`.
        let batch = json!([
            {"id": "own", "tool": "w.timeline", "where": ["result=failed"]},
            {"id": "default", "tool": "w.timeline"},
            {"id": "page", "tool": "w.list_items", "args": {"per_page": 2}, "where": ["id=1"]},
            {"id": "none", "tool": "w.timeline", "where": ["result=failure"]},
        ]);
        let file = root.join("where-batch.json");
        std::fs::write(&file, batch.to_string()).unwrap();
        let file = file.display().to_string();
        let all = with_mode(&["batch", "--file", &file, "--where", "type=Job"]);
        let results: Value = serde_json::from_str(&run_ok(
            &env,
            &all.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
        .unwrap();
        assert_eq!(
            names(&results[0]["result"]["records"]),
            ["a"],
            "{label}: {results}"
        );
        assert_eq!(
            names(&results[1]["result"]["records"]),
            ["b"],
            "{label}: {results}"
        );
        assert_eq!(results[2]["result"], json!([{"id": 1, "name": "a"}]));
        assert_eq!(results[2]["next_page"], json!({"page": "2"}), "{label}");
        assert!(results[2].get("note").is_none(), "{label}: {results}");
        assert_eq!(results[3]["result"]["records"], json!([]), "{label}");
        let note = results[3]["note"].as_str().unwrap_or_default();
        assert!(
            note.contains("kept none of the 4 items in records") && note.contains(r#""failed""#),
            "{label}: {results}"
        );

        // A bad condition stops the batch before anything is called, naming the item.
        std::fs::write(
            &file,
            json!([{"id": "bad", "tool": "w.timeline", "where": ["oops"]}]).to_string(),
        )
        .unwrap();
        let all = with_mode(&["batch", "--file", &file]);
        let e = run_err(&env, &all.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(
            e.contains("batch item 'bad'") && e.contains("oops"),
            "{label}: {e}"
        );

        // Batch input: unknown keys are refused, ids default to the position and must be unique,
        // and a failed item makes the exit code 2 (the results are still printed).
        let run_batch = |items: Value| {
            std::fs::write(&file, items.to_string()).unwrap();
            env.cmd()
                .args(with_mode(&["batch", "--file", &file]))
                .output()
                .unwrap()
        };
        let out = run_batch(json!([{"tool": "w.timeline", "feilds": ["id"]}]));
        let e = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.code() == Some(1) && e.contains("unknown field `feilds`"),
            "{label}: {e}"
        );
        let out = run_batch(json!([
            {"tool": "w.timeline", "arguments": {}, "fields": ["nmae"]},
            {"tool": "w.one"},
        ]));
        assert!(out.status.success(), "{label}: {out:?}");
        let results: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(results[0]["id"], "0", "{label}: {results}");
        assert_eq!(results[1]["id"], "1", "{label}: {results}");
        assert!(
            results[0]["note"]
                .as_str()
                .is_some_and(|n| n.starts_with("--fields 'nmae' matched nothing (keys here:")),
            "{label}: {results}"
        );
        // "where" and "fields" also take one string, as on the command line.
        let out = run_batch(json!([
            {"tool": "w.timeline", "where": "records.result=failed", "fields": "records.name,records.result"},
        ]));
        assert!(out.status.success(), "{label}: {out:?}");
        let results: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            results[0]["result"],
            json!({"records": [{"name": "a", "result": "failed"}]}),
            "{label}: {results}"
        );
        let out = run_batch(json!([{"tool": "w.timeline", "where": 5}]));
        let e = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.code() == Some(1)
                && e.contains("\"where\" takes a condition or a list of them"),
            "{label}: {e}"
        );
        let out = run_batch(json!([
            {"id": "a", "tool": "w.timeline"},
            {"id": "a", "tool": "w.one"},
        ]));
        let e = String::from_utf8_lossy(&out.stderr);
        assert!(
            e.contains("batch items 0 and 1 have the same id 'a'"),
            "{label}: {e}"
        );
        let out = run_batch(json!([
            {"id": "good", "tool": "w.timeline"},
            {"id": "bad", "tool": "w.nope"},
        ]));
        assert_eq!(out.status.code(), Some(2), "{label}: {out:?}");
        let results: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(results[1]["ok"], false, "{label}: {results}");
        let e = String::from_utf8_lossy(&out.stderr);
        assert!(
            e.contains("1 of 2 batch items failed (ids: bad)"),
            "{label}: {e}"
        );
    }
}

/// A daemon from before `--where` would answer as if no filter had been given, so the CLI refuses
/// (nothing is called) and says to restart it. `daemon status` names the old version.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn where_refuses_a_daemon_that_does_not_know_it() {
    let env = IsolatedEnv::new();
    let base = spawn_where_api().await;
    let manual = json!({
        "manual_version": "1.0.0",
        "utcp_version": "1.0.0",
        "info": {"title": "w", "version": "1"},
        "tools": [get_tool("timeline", format!("{base}/timeline"))],
    });
    let path = env.tmp.path().join("w.utcp.json");
    std::fs::write(&path, manual.to_string()).unwrap();
    run_ok(&env, &["register", "w", &path.display().to_string()]);

    // What a 0.2.3 daemon answers to Status: this build's answer without `version` and `features`.
    run_ok(&env, &["daemon", "start"]);
    let mut old: Value =
        serde_json::from_str(&run_ok(&env, &["daemon", "status", "--json"])).unwrap();
    run_ok(&env, &["daemon", "stop"]);
    let obj = old.as_object_mut().unwrap();
    obj.remove("version");
    obj.remove("features");
    let reply = json!({"status": "ok", "data": {"kind": "daemon_status", "value": old}});
    let _ = std::fs::remove_file(env.tmp.path().join("utcp-test.sock"));
    let server = capture_ipc_requests(&env, vec![reply.clone(), reply.clone(), reply]);

    let e = run_err(&env, &["call", "w.timeline", "--where", "result=failed"]);
    assert!(
        e.contains("0.2.3 or older")
            && e.contains("nothing was called")
            && e.contains("utcp daemon restart"),
        "{e}"
    );
    let file = env.tmp.path().join("b.json");
    std::fs::write(
        &file,
        json!([{"id": "a", "tool": "w.timeline"}]).to_string(),
    )
    .unwrap();
    let e = run_err(
        &env,
        &[
            "batch",
            "--file",
            &file.display().to_string(),
            "--where",
            "type=Job",
        ],
    );
    assert!(e.contains("does not know --where"), "{e}");
    let text = run_ok(&env, &["daemon", "status"]);
    assert!(
        text.contains("Version      : 0.2.3 or older") && text.contains("WARNING"),
        "{text}"
    );

    let seen = server.join().unwrap();
    let kinds: Vec<_> = seen.iter().map(|r| r["command"]["type"].clone()).collect();
    assert_eq!(
        kinds,
        ["status", "status", "status"],
        "no call reached the daemon"
    );
}
