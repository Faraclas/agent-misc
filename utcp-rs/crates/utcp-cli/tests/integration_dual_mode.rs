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
    assert!(run_ok(&env, &["list"]).contains("pets."));

    // 1. Deregister: gone from list/search, but entry and saved copy are kept.
    let out = run_ok(&env, &["deregister", "pets"]);
    assert!(
        out.contains("utcp register pets"),
        "restore hint missing: {out}"
    );
    assert!(!run_ok(&env, &["list"]).contains("pets."));
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
    assert!(run_ok(&env, &["list"]).contains("pets."));
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
    assert!(!run_ok(&env, &["list"]).contains("stray."));
    let err = run_err(&env, &["register", "nope"]);
    assert!(err.contains("available: stray"), "{err}");
    run_ok(&env, &["register", "stray"]);
    assert!(run_ok(&env, &["list"]).contains("stray."));
    run_ok(&env, &["deregister", "stray", "--purge-cache"]);
    assert!(!stray.exists());
    assert!(!run_ok(&env, &["list"]).contains("stray."));
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
    assert!(run_ok(&env, &["list", "--manual", "authy"]).contains("authy.ping"));

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
    assert!(run_ok(&env, &["list", "--manual", "privy"]).contains("privy.ping"));
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
