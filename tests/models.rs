use checkweave::models::*;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

struct EnvVar {
    key: &'static str,
    prev: Option<String>,
}
impl EnvVar {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, prev }
    }
}
impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn write_exe(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

fn provenance_json() -> String {
    format!(
        r#"{{"provider":"local","model":"{LOCAL_MODEL_ID}","revision":"{LOCAL_MODEL_REVISION}","adapter_version":"{GLINER_ADAPTER_VERSION}","device":"cpu","precision":"float32","score_semantics":{{"choice":"exclusive_softmax"}},"input_policy":"reject_over_limit;predicate_unsupported","runtime_versions":{{"python":"test"}},"profile":"lightweight","backend":"gliner2"}}"#
    )
}

fn semif_provenance_json() -> String {
    format!(
        r#"{{"provider":"local","model":"{SEMIF_MODEL_ID}","revision":"{SEMIF_MODEL_REVISION}","adapter_version":"{SEMIF_ADAPTER_VERSION}","device":"cpu","precision":"gguf-q4_k_m","score_semantics":{{"predicate":"exclusive_softmax_over_supported_insufficient_contradicted"}},"input_policy":"semif_option_logit","runtime_versions":{{"semif_code":"{SEMIF_CODE_REVISION}"}},"device_fallback":"weights do not fit; retained cpu","profile":"default","backend":"semif"}}"#
    )
}

fn workspace(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    if !text.is_empty() {
        fs::write(dir.path().join("checkweave.toml"), text).unwrap();
    }
    dir
}

fn local_toml() -> String {
    format!(
        "[model]\nprovider = \"local\"\ndevice = \"cpu\"\nthreads = 2\nstartup_timeout_ms = 2000\nidle_timeout_ms = 600000\ncheckpoint = \"{LOCAL_MODEL_ID}\"\nrevision = \"{LOCAL_MODEL_REVISION}\"\n"
    )
}

fn choice() -> ModelQuestion {
    ModelQuestion::Choice {
        id: "q".into(),
        labels: vec![
            ChoiceLabel {
                label: "a".into(),
                description: "alpha".into(),
            },
            ChoiceLabel {
                label: "b".into(),
                description: "beta".into(),
            },
        ],
    }
}

fn ordinal() -> ModelQuestion {
    ModelQuestion::Ordinal {
        id: "rank".into(),
        levels: vec![
            OrdinalLevel {
                label: "low".into(),
                description: "low".into(),
                value: 0.0,
            },
            OrdinalLevel {
                label: "high".into(),
                description: "high".into(),
                value: 1.0,
            },
        ],
    }
}

fn state() -> ModelState {
    ModelState {
        id: "record-1".into(),
        text: "The payout failed.".into(),
    }
}

fn cancel_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn missing_file_defaults_to_pinned_local_without_auth() {
    let dir = workspace("");
    let loaded = load_model_config(dir.path()).unwrap();
    assert_eq!(loaded.config.provider, ModelProviderKind::Local);
    assert_eq!(loaded.config.profile, Some(LocalProfile::Default));
    assert_eq!(loaded.config.checkpoint.as_deref(), Some(SEMIF_MODEL_ID));
    assert_eq!(
        loaded.config.revision.as_deref(),
        Some(SEMIF_MODEL_REVISION)
    );
    let gliner =
        parse_workspace_toml(&format!("[model]\ncheckpoint = \"{LOCAL_MODEL_ID}\"\n")).unwrap();
    assert_eq!(gliner.config.profile, Some(LocalProfile::Lightweight));
    assert_eq!(
        gliner.config.revision.as_deref(),
        Some(LOCAL_MODEL_REVISION)
    );
    assert!(loaded.config.api_key_env.is_none());
    assert!(loaded.config.endpoint.is_none());
    assert_eq!(loaded.config.threads, 4);
    assert_eq!(loaded.config.device, "auto");
    assert!(!loaded.source_fingerprint.is_empty());
    assert!(!loaded.settings_fingerprint.is_empty());
}

#[test]
fn invalid_model_table_is_an_error() {
    let err = parse_workspace_toml("[model]\nprovider = \"cloud\"\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cloud") || err.contains("invalid"), "{err}");
    let secret = "super-secret-should-not-appear";
    let err = parse_workspace_toml(&format!("[model]\napi_key = \"{secret}\"\n"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("api_key"), "{err}");
    assert!(!err.contains(secret), "{err}");
    let err = parse_workspace_toml(
        "[model]\nprovider = \"jev\"\nmodel = \"jev-latest\"\napi_key_env = \"TYPESAFE_API_KEY\"\n",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("jev-latest"), "{err}");
    let err = parse_workspace_toml("[model]\nprovider = \"jev\"\nmodel = \"jev-preview\"\napi_key_env = \"TYPESAFE_API_KEY\"\n").unwrap_err().to_string();
    assert!(err.contains("jev-preview"), "{err}");
    assert!(parse_workspace_toml("[model]\nprovider = \"jev\"\nmodel = \"jev-1.13.0\"\n").is_err());
    assert!(
        parse_workspace_toml(
            "[model]\nprovider = \"local\"\nendpoint = \"http://127.0.0.1:9/v1/systemone\"\n"
        )
        .is_err()
    );
    assert!(parse_workspace_toml("[model]\ncheckpoint = \"other/model\"\n").is_err());
    let loaded = parse_workspace_toml(
        "[model]\nprovider = \"jev\"\nmodel = \"jev-1.13.0\"\napi_key_env = \"TYPESAFE_API_KEY\"\n",
    )
    .unwrap();
    assert_eq!(loaded.config.model.as_deref(), Some(JEV_MODEL_ID));
    assert_eq!(loaded.config.endpoint.as_deref(), Some(JEV_ENDPOINT));
    assert!(!loaded.settings_fingerprint.contains("TYPESAFE"));
    let changed =
        parse_workspace_toml("[model]\nprovider = \"local\"\ndevice = \"cpu\"\n").unwrap();
    let again = parse_workspace_toml("[model]\nprovider = \"local\"\ndevice = \"cpu\"\n").unwrap();
    assert_eq!(changed.settings_fingerprint, again.settings_fingerprint);
    assert_ne!(changed.settings_fingerprint, loaded.settings_fingerprint);
}

#[test]
fn redaction_removes_bearer_and_secret() {
    let text = "Authorization: Bearer sk-live-token leaked sk-live-token";
    let cleaned = redact_sensitive(text, "sk-live-token");
    assert!(!cleaned.contains("sk-live-token"), "{cleaned}");
    assert!(cleaned.contains("[redacted]"));
}

#[tokio::test]
async fn predicate_only_does_not_start_python() {
    let _lock = env_lock().await;
    let root = workspace(&local_toml());
    let cache = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let marker = cache.path().join("launched");
    let script = format!("#!/bin/sh\necho launched >> {}\nexit 1\n", marker.display());
    let python = write_exe(bin.path(), "python-mock", &script);
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", python.to_str().unwrap());
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let response = provider
        .evaluate(
            vec![state()],
            vec![ModelQuestion::Predicate {
                id: "p".into(),
                statement: "The tests passed on Windows.".into(),
            }],
            2_000,
            cancel_flag(),
        )
        .await
        .unwrap();
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].status, ModelStatus::Unsupported);
    assert!(
        response.results[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("does not support")
    );
    assert!(response.results[0].scores.is_none());
    assert_eq!(response.provenance.device, "not_invoked");
    assert_eq!(response.provenance.model, LOCAL_MODEL_ID);
    assert_eq!(provider.config().provider, ModelProviderKind::Local);
    assert!(!provider.source_fingerprint().is_empty());
    assert_eq!(provider.settings_identity().revision, LOCAL_MODEL_REVISION);
    assert!(!marker.exists());
    assert!(provider.managed_worker_pid().is_none());
}

#[tokio::test]
async fn mock_worker_resolves_choice_and_restarts_once_after_crash() {
    let _lock = env_lock().await;
    let root = workspace(&local_toml());
    let cache = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let counter = cache.path().join("count");
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, sys
path = {counter:?}
n = int(open(path).read()) if os.path.exists(path) else 0
n += 1
open(path, "w").write(str(n))
if n == 1:
    sys.exit(1)
prov = json.loads({prov:?})
sys.stdout.write(json.dumps({{"version": 1, "ready": True, "provenance": prov}}) + "\n")
sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    results = []
    for state in req["states"]:
        for question in req["questions"]:
            label = "a" if question["kind"] == "choice" else "low"
            value = 0.25 if question["kind"] == "ordinal" else None
            results.append({{"state_id": state["id"], "question_id": question["id"], "status": "resolved", "label": label, "value": value, "scores": {{"a": 0.8}}}})
    sys.stdout.write(json.dumps({{"version": 1, "id": req["id"], "results": results, "provenance": prov}}) + "\n")
    sys.stdout.flush()
"#,
        counter = counter.display().to_string(),
        prov = provenance_json(),
    );
    let python = write_exe(bin.path(), "python-mock", &script);
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", python.to_str().unwrap());
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let response = provider
        .evaluate(
            vec![state()],
            vec![choice(), ordinal()],
            3_000,
            cancel_flag(),
        )
        .await
        .unwrap();
    assert_eq!(response.results.len(), 2);
    assert_eq!(response.results[0].label.as_deref(), Some("a"));
    assert_eq!(response.results[1].label.as_deref(), Some("low"));
    assert_eq!(response.results[1].value, Some(serde_json::json!(0.25)));
    assert_eq!(response.provenance.revision, LOCAL_MODEL_REVISION);
    assert_eq!(fs::read_to_string(&counter).unwrap().trim(), "2");
    let key = result_cache_key(provider.settings_fingerprint(), &response.provenance);
    assert_eq!(
        key,
        result_cache_key(provider.settings_fingerprint(), &response.provenance)
    );
    assert_ne!(key, provider.settings_fingerprint());
}

#[tokio::test]
async fn worker_crash_twice_is_an_error() {
    let _lock = env_lock().await;
    let root = workspace(&local_toml());
    let cache = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let python = write_exe(bin.path(), "python-mock", "#!/bin/sh\nexit 1\n");
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", python.to_str().unwrap());
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = provider
        .evaluate(vec![state()], vec![choice()], 2_000, cancel_flag())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("crashed"), "{err}");
}

#[tokio::test]
async fn cancellation_reaps_the_worker() {
    let _lock = env_lock().await;
    let root = workspace(&local_toml());
    let cache = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let script = format!(
        r#"#!/usr/bin/env python3
import json, sys, time
prov = json.loads({prov:?})
sys.stdout.write(json.dumps({{"version": 1, "ready": True, "provenance": prov}}) + "\n")
sys.stdout.flush()
sys.stdin.readline()
time.sleep(30)
"#,
        prov = provenance_json(),
    );
    let python = write_exe(bin.path(), "python-mock", &script);
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", python.to_str().unwrap());
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let cancel = cancel_flag();
    let flag = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        flag.store(true, Ordering::Release);
    });
    let err = tokio::time::timeout(
        Duration::from_secs(4),
        provider.evaluate(vec![state()], vec![choice()], 10_000, cancel),
    )
    .await
    .expect("cancel hung")
    .unwrap_err()
    .to_string();
    assert!(err.contains("cancel"), "{err}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(provider.managed_worker_pid().is_none());
}

#[tokio::test]
async fn startup_timeout_reaps_the_worker() {
    let _lock = env_lock().await;
    let root = workspace(
        "[model]\ndevice = \"cpu\"\nthreads = 1\nstartup_timeout_ms = 300\nidle_timeout_ms = 600000\n",
    );
    let cache = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let python = write_exe(bin.path(), "python-mock", "#!/bin/sh\nsleep 30\n");
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", python.to_str().unwrap());
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = tokio::time::timeout(
        Duration::from_secs(4),
        provider.evaluate(vec![state()], vec![choice()], 5_000, cancel_flag()),
    )
    .await
    .expect("startup timeout hung")
    .unwrap_err()
    .to_string();
    assert!(err.contains("timed out"), "{err}");
    assert!(provider.managed_worker_pid().is_none());
}

#[tokio::test]
async fn oversized_frame_is_rejected_before_launch() {
    let _lock = env_lock().await;
    let root = workspace(&local_toml());
    let cache = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let marker = cache.path().join("launched");
    let script = format!("#!/bin/sh\necho launched >> {}\nexit 1\n", marker.display());
    let python = write_exe(bin.path(), "python-mock", &script);
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", python.to_str().unwrap());
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let huge = ModelState {
        id: "big".into(),
        text: "x".repeat(MAX_FRAME_BYTES),
    };
    let err = provider
        .evaluate(vec![huge], vec![choice()], 2_000, cancel_flag())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("8MiB"), "{err}");
    assert!(!marker.exists());
}

#[tokio::test]
async fn offline_setup_requires_a_completed_manifest_and_uv_install_is_reused() {
    let _lock = env_lock().await;
    let cache = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let log = cache.path().join("uv.log");
    let script = format!(
        r#"#!/bin/sh
echo "$@" >> {log}
if [ "$1" = "venv" ]; then
  dest=""
  for arg in "$@"; do dest="$arg"; done
  mkdir -p "$dest/bin"
  printf '#!/bin/sh\nif [ "$1" = "-c" ]; then echo 3.12; exit 0; fi\nexit 0\n' > "$dest/bin/python"
  chmod +x "$dest/bin/python"
  exit 0
fi
exit 0
"#,
        log = log.display(),
    );
    let _uv = write_exe(bin.path(), "uv", &script);
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let path_value = format!("{}:/usr/bin:/bin", bin.path().display());
    let _path = EnvVar::set("PATH", &path_value);
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", "");
    unsafe {
        std::env::remove_var("CHECKWEAVE_PYTHON");
    }
    let _req = EnvVar::set("CHECKWEAVE_REQUIREMENTS_FILE", "");
    unsafe {
        std::env::remove_var("CHECKWEAVE_REQUIREMENTS_FILE");
    }
    let config = ModelConfig::default();
    let err = setup(&config, true).await.unwrap_err().to_string();
    assert!(err.contains("offline"), "{err}");
    let first = setup(&config, false).await.unwrap();
    assert_eq!(first["installer"], "uv");
    assert_eq!(first["model"], SEMIF_MODEL_ID);
    assert_eq!(first["profile"], "default");
    let calls = fs::read_to_string(&log).unwrap().lines().count();
    let second = setup(&config, true).await.unwrap();
    assert_eq!(second["requirements_sha256"], first["requirements_sha256"]);
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        calls,
        "second setup should reuse the manifest"
    );
    assert!(calls >= 2, "venv plus requirements install, got {calls}");
}

struct Reply {
    status: u16,
    retry_after: Option<&'static str>,
    body: String,
    delay_ms: u64,
}

struct Captured {
    headers: String,
    body: String,
}

async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let store = captured.clone();
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let reply = match replies.next() {
                Some(reply) => reply,
                None => Reply {
                    status: 500,
                    retry_after: None,
                    body: "{\"error\":\"extra\"}".into(),
                    delay_ms: 0,
                },
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let (header, body) = loop {
                let n = socket.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&buf[..pos]).into_owned();
                    let length = header
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let mut body = buf[pos + 4..].to_vec();
                    while body.len() < length {
                        let n = socket.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&tmp[..n]);
                    }
                    body.truncate(length);
                    break (header, body);
                }
            };
            store.lock().unwrap().push(Captured {
                headers: header,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
            if reply.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(reply.delay_ms)).await;
            }
            let mut response = format!(
                "HTTP/1.1 {} TEST\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                reply.status,
                reply.body.len()
            );
            if let Some(after) = reply.retry_after {
                response.push_str(&format!("Retry-After: {after}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&reply.body);
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://127.0.0.1:{port}/v1/systemone"), captured)
}

fn jev_success(model: &str, answers: &str, usage: &str) -> String {
    format!(r#"{{"model":"{model}","answers":{answers},"usage":{usage}}}"#)
}

fn jev_config(endpoint: &str) -> String {
    format!(
        "[model]\nprovider = \"jev\"\nmodel = \"jev-1.13.0\"\napi_key_env = \"CHECKWEAVE_TEST_JEV_KEY\"\nendpoint = \"{endpoint}\"\n"
    )
}

#[tokio::test]
async fn jev_retries_429_and_503_and_keeps_concrete_scores() {
    let _lock = env_lock().await;
    let answers = r#"{
        "urgent": {"type": "noul", "noul": 0.95},
        "team": {"type": "choice", "choice": "billing", "probabilities": {"billing": 0.8, "technical": 0.2}, "confidence": 0.81},
        "mood": {"type": "score", "score": 1.05, "probabilities": {"0": 0.0, "1": 0.95, "2": 0.05}, "confidence": 0.92}
    }"#;
    let ok = jev_success(
        JEV_MODEL_ID,
        answers,
        r#"{"input_tokens": 10, "output_tokens": 2}"#,
    );
    let (endpoint, captured) = serve(vec![
        Reply {
            status: 429,
            retry_after: Some("0"),
            body: "{\"error\":\"slow\"}".into(),
            delay_ms: 0,
        },
        Reply {
            status: 503,
            retry_after: Some("0"),
            body: "{\"error\":\"unavailable\"}".into(),
            delay_ms: 0,
        },
        Reply {
            status: 200,
            retry_after: None,
            body: ok,
            delay_ms: 0,
        },
    ])
    .await;
    let root = workspace(&jev_config(&endpoint));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", "test-key-not-logged");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let questions = vec![
        ModelQuestion::Predicate {
            id: "urgent".into(),
            statement: "Does this convey urgency?".into(),
        },
        ModelQuestion::Choice {
            id: "team".into(),
            labels: vec![
                ChoiceLabel {
                    label: "billing".into(),
                    description: "payments".into(),
                },
                ChoiceLabel {
                    label: "technical".into(),
                    description: "bugs".into(),
                },
            ],
        },
        ModelQuestion::Ordinal {
            id: "mood".into(),
            levels: vec![
                OrdinalLevel {
                    label: "calm".into(),
                    description: "Calm".into(),
                    value: 0.0,
                },
                OrdinalLevel {
                    label: "frustrated".into(),
                    description: "Frustrated".into(),
                    value: 1.0,
                },
                OrdinalLevel {
                    label: "angry".into(),
                    description: "Very angry".into(),
                    value: 2.0,
                },
            ],
        },
    ];
    let response = provider
        .evaluate(vec![state()], questions, 5_000, cancel_flag())
        .await
        .unwrap();
    assert_eq!(captured.lock().unwrap().len(), 3);
    let body: serde_json::Value = serde_json::from_str(&captured.lock().unwrap()[2].body).unwrap();
    assert_eq!(body["model"], "jev-1.13.0");
    assert!(body["questions"]["urgent"]["type"] == "noul");
    assert!(body["questions"]["team"]["type"] == "choice");
    assert!(body["questions"]["mood"]["type"] == "score");
    assert_eq!(body["questions"]["mood"]["criteria"][2], "Very angry");
    assert!(
        captured.lock().unwrap()[0]
            .headers
            .contains("Bearer test-key-not-logged")
    );
    assert_eq!(response.provenance.model, JEV_MODEL_ID);
    assert_eq!(response.provenance.revision, JEV_MODEL_ID);
    assert_eq!(response.provenance.usage.as_ref().unwrap().input_tokens, 10);
    assert_eq!(response.results[0].label.as_deref(), Some("true"));
    assert_eq!(response.results[0].value, Some(serde_json::json!(0.95)));
    assert_eq!(response.results[1].label.as_deref(), Some("billing"));
    assert_eq!(response.results[1].confidence, Some(0.81));
    assert_eq!(response.results[2].label.as_deref(), Some("frustrated"));
    assert!(
        (response.results[2]
            .value
            .as_ref()
            .unwrap()
            .as_f64()
            .unwrap()
            - 1.05)
            .abs()
            < 1e-9
    );
    assert_eq!(response.results[2].provider_value, Some(1.05));
    let semantics = response.provenance.score_semantics.as_str().unwrap();
    assert!(semantics.contains("noul_yes_probability"));
    assert!(semantics.contains("choice_exclusive_probabilities"));
    let key = result_cache_key(provider.settings_fingerprint(), &response.provenance);
    assert!(!key.is_empty());
}

#[tokio::test]
async fn jev_does_not_retry_status_500_and_redacts_the_key() {
    let _lock = env_lock().await;
    let (endpoint, captured) = serve(vec![
        Reply {
            status: 500,
            retry_after: Some("0"),
            body: "leaked test-key-not-logged in body".into(),
            delay_ms: 0,
        },
        Reply {
            status: 200,
            retry_after: None,
            body: jev_success(
                JEV_MODEL_ID,
                r#"{"urgent":{"type":"noul","noul":0.1}}"#,
                r#"{"input_tokens":1,"output_tokens":1}"#,
            ),
            delay_ms: 0,
        },
    ])
    .await;
    let root = workspace(&jev_config(&endpoint));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", "test-key-not-logged");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = provider
        .evaluate(
            vec![state()],
            vec![ModelQuestion::Predicate {
                id: "urgent".into(),
                statement: "Is it urgent?".into(),
            }],
            3_000,
            cancel_flag(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("500"), "{err}");
    assert!(!err.contains("test-key-not-logged"), "{err}");
    assert_eq!(captured.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn jev_rejects_alias_response_and_invalid_questions_without_a_call() {
    let _lock = env_lock().await;
    let (endpoint, captured) = serve(vec![Reply {
        status: 200,
        retry_after: None,
        body: jev_success(
            "jev-latest",
            r#"{"team":{"type":"choice","choice":"a","probabilities":{"a":1}}}"#,
            r#"{"input_tokens":1,"output_tokens":1}"#,
        ),
        delay_ms: 0,
    }])
    .await;
    let root = workspace(&jev_config(&endpoint));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", "test-key-not-logged");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = provider
        .evaluate(vec![state()], vec![choice()], 3_000, cancel_flag())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a pinned"), "{err}");
    let before = captured.lock().unwrap().len();
    let err = provider
        .evaluate(
            vec![state()],
            vec![ModelQuestion::Choice {
                id: "q".into(),
                labels: vec![],
            }],
            3_000,
            cancel_flag(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("choice"), "{err}");
    assert_eq!(captured.lock().unwrap().len(), before);
}

#[tokio::test]
async fn jev_cancel_during_retry_after_does_not_send_again() {
    let _lock = env_lock().await;
    let (endpoint, captured) = serve(vec![
        Reply {
            status: 429,
            retry_after: Some("30"),
            body: "{}".into(),
            delay_ms: 0,
        },
        Reply {
            status: 200,
            retry_after: None,
            body: "{}".into(),
            delay_ms: 0,
        },
    ])
    .await;
    let root = workspace(&jev_config(&endpoint));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", "test-key-not-logged");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let cancel = cancel_flag();
    let flag = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        flag.store(true, Ordering::Release);
    });
    let err = tokio::time::timeout(
        Duration::from_secs(3),
        provider.evaluate(vec![state()], vec![choice()], 10_000, cancel),
    )
    .await
    .expect("retry sleep hung")
    .unwrap_err()
    .to_string();
    assert!(err.contains("cancel"), "{err}");
    assert_eq!(captured.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn jev_two_states_make_two_requests_and_sum_usage() {
    let _lock = env_lock().await;
    let body = jev_success(
        JEV_MODEL_ID,
        r#"{"q":{"type":"choice","choice":"a","probabilities":{"a":0.8,"b":0.2},"confidence":0.5}}"#,
        r#"{"input_tokens":4,"output_tokens":1}"#,
    );
    let (endpoint, captured) = serve(vec![
        Reply {
            status: 200,
            retry_after: None,
            body: body.clone(),
            delay_ms: 0,
        },
        Reply {
            status: 200,
            retry_after: None,
            body,
            delay_ms: 0,
        },
    ])
    .await;
    let root = workspace(&jev_config(&endpoint));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", "test-key-not-logged");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let response = provider
        .evaluate(
            vec![
                state(),
                ModelState {
                    id: "record-2".into(),
                    text: "second".into(),
                },
            ],
            vec![choice()],
            3_000,
            cancel_flag(),
        )
        .await
        .unwrap();
    assert_eq!(captured.lock().unwrap().len(), 2);
    assert_eq!(response.results.len(), 2);
    assert_eq!(response.provenance.usage.unwrap().input_tokens, 8);
    assert_eq!(response.results[0].state_id, "record-1");
    assert_eq!(response.results[1].state_id, "record-2");
}

#[test]
fn settings_identity_records_pins_without_secrets() {
    let loaded = parse_workspace_toml("[model]\nprovider = \"jev\"\nmodel = \"jev-1.13.0\"\napi_key_env = \"TYPESAFE_API_KEY\"\nendpoint = \"http://127.0.0.1:9/v1/systemone\"\n").unwrap();
    let identity = settings_identity(&loaded.config);
    assert_eq!(identity.model, "jev-1.13.0");
    assert_eq!(identity.revision, "jev-1.13.0");
    assert_eq!(identity.api_key_env.as_deref(), Some("TYPESAFE_API_KEY"));
    assert_eq!(
        identity.endpoint_origin.as_deref(),
        Some("http://127.0.0.1:9")
    );
    assert_eq!(identity.device, "remote");
    let rendered = serde_json::to_string(&identity).unwrap();
    assert!(!rendered.contains("sk-"));
}

#[tokio::test]
async fn semif_forwards_predicates_and_readiness_skips_inference() {
    let _lock = env_lock().await;
    let root = workspace("[model]\ndevice = \"cpu\"\nthreads = 1\nstartup_timeout_ms = 2000\n");
    let cache = tempfile::tempdir().unwrap();
    let seen = cache.path().join("kinds");
    let script = format!(
        r#"#!/usr/bin/env python3
import json, sys
prov = json.loads({prov:?})
sys.stdout.write(json.dumps({{"version": 1, "ready": True, "provenance": prov}}) + "\n")
sys.stdout.flush()
for line in sys.stdin:
    if not line.strip():
        continue
    req = json.loads(line)
    kinds = ",".join(question["kind"] for question in req["questions"])
    open({seen:?}, "w").write(kinds)
    results = []
    for state in req["states"]:
        for question in req["questions"]:
            results.append({{"state_id": state["id"], "question_id": question["id"], "status": "resolved", "label": "supported", "value": True, "scores": {{"supported": 0.7, "insufficient": 0.2, "contradicted": 0.1}}}})
    sys.stdout.write(json.dumps({{"version": 1, "id": req["id"], "results": results, "provenance": prov}}) + "\n")
    sys.stdout.flush()
"#,
        prov = semif_provenance_json(),
        seen = seen.display().to_string(),
    );
    let python = write_exe(cache.path(), "python-mock", &script);
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _python = EnvVar::set("CHECKWEAVE_PYTHON", python.to_str().unwrap());
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    assert_eq!(provider.config().profile, Some(LocalProfile::Default));
    let ready = provider.readiness(2_000, cancel_flag()).await.unwrap();
    assert_eq!(ready.model, SEMIF_MODEL_ID);
    assert_eq!(ready.adapter_version, SEMIF_ADAPTER_VERSION);
    assert_eq!(
        ready.device_fallback.as_deref(),
        Some("weights do not fit; retained cpu")
    );
    assert!(!seen.exists());
    let again = provider.readiness(2_000, cancel_flag()).await.unwrap();
    assert_eq!(again.device, ready.device);
    let response = provider
        .evaluate(
            vec![state()],
            vec![ModelQuestion::Predicate {
                id: "p".into(),
                statement: "The payout failed.".into(),
            }],
            2_000,
            cancel_flag(),
        )
        .await
        .unwrap();
    assert_eq!(response.results[0].status, ModelStatus::Resolved);
    assert_eq!(
        response.results[0].value,
        Some(serde_json::Value::Bool(true))
    );
    assert_eq!(fs::read_to_string(&seen).unwrap(), "predicate");
    let mut changed = response.provenance.clone();
    changed.device_fallback = Some("different fallback".into());
    assert_ne!(
        result_cache_key(provider.settings_fingerprint(), &response.provenance),
        result_cache_key(provider.settings_fingerprint(), &changed),
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "installs the pinned SemIf CPU stack and runs the canonical worker"]
async fn installed_canonical_worker_cpu_smoke() {
    let _lock = env_lock().await;
    let cache = tempfile::tempdir().unwrap();
    let hub = std::env::var_os("HF_HUB_CACHE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/huggingface/hub"))
        })
        .unwrap_or_default();
    if hub.is_dir() {
        let dest = cache.path().join("huggingface");
        fs::create_dir_all(&dest).unwrap();
        for name in [
            "models--bartowski--Qwen_Qwen3.5-4B-GGUF",
            "models--Qwen--Qwen3.5-4B",
        ] {
            let src = hub.join(name);
            if src.exists() {
                std::os::unix::fs::symlink(&src, dest.join(name)).unwrap();
            }
        }
    }
    let _cache = EnvVar::set("CHECKWEAVE_CACHE_DIR", cache.path().to_str().unwrap());
    let _install = EnvVar::set("CHECKWEAVE_INSTALL_TIMEOUT_MS", "3600000");
    unsafe {
        std::env::remove_var("CHECKWEAVE_PYTHON");
        std::env::remove_var("CHECKWEAVE_REQUIREMENTS_FILE");
    }
    let config = ModelConfig {
        device: "cpu".into(),
        threads: 4,
        startup_timeout_ms: 600_000,
        ..ModelConfig::default()
    };
    let installed = setup(&config, false).await.expect("pinned profile setup");
    assert_eq!(installed["profile"], "default");
    assert_eq!(installed["model"], SEMIF_MODEL_ID);
    assert_eq!(installed["managed_env"], true);
    let root = workspace("[model]\ndevice = \"cpu\"\nthreads = 4\nstartup_timeout_ms = 600000\n");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let ready = provider
        .readiness(600_000, cancel_flag())
        .await
        .expect("worker ready");
    assert_eq!(ready.model, SEMIF_MODEL_ID);
    assert_eq!(ready.revision, SEMIF_MODEL_REVISION);
    assert_eq!(ready.adapter_version, SEMIF_ADAPTER_VERSION);
    assert_eq!(ready.provider, "local");
    assert_eq!(ready.precision, "gguf-q4_k_m");
    assert_eq!(ready.backend.as_deref(), Some("semif"));
    assert_eq!(
        ready.gguf_sha256.as_deref(),
        Some("13c16f426047e2de38cd075bdade4a7bcbc8c774384876f677740cda65f8a983"),
    );
    assert!(!ready.device.is_empty());
    let response = provider
        .evaluate(vec![state()], vec![choice()], 600_000, cancel_flag())
        .await
        .expect("cpu evaluate");
    assert_eq!(response.results.len(), 1);
    assert_ne!(response.results[0].status, ModelStatus::Unsupported);
    assert_eq!(response.provenance.adapter_version, SEMIF_ADAPTER_VERSION);
    eprintln!(
        "smoke device={} fallback={:?} precision={}",
        response.provenance.device,
        response.provenance.device_fallback,
        response.provenance.precision
    );
}

#[test]
fn torch_wheel_follows_the_bf16_gate_and_does_not_collide() {
    let m10 = VisibleGpu {
        index: 0,
        free_bytes: 8 * 1024 * 1024 * 1024,
        compute_major: 5,
    };
    let four = vec![
        m10.clone(),
        VisibleGpu {
            index: 1,
            ..m10.clone()
        },
        VisibleGpu {
            index: 2,
            ..m10.clone()
        },
        VisibleGpu { index: 3, ..m10 },
    ];
    assert_eq!(
        select_semif_torch_wheel("auto", &four, "linux").unwrap(),
        TorchWheel::Cpu
    );
    assert!(select_semif_torch_wheel("cuda", &four, "linux").is_err());
    assert!(select_semif_torch_wheel("cuda:1", &four, "linux").is_err());
    let capable = VisibleGpu {
        index: 0,
        free_bytes: 20 * 1024 * 1024 * 1024,
        compute_major: 8,
    };
    let mixed = vec![
        four[0].clone(),
        VisibleGpu {
            index: 1,
            ..capable
        },
    ];
    assert_eq!(
        select_semif_torch_wheel("auto", &mixed, "linux").unwrap(),
        TorchWheel::CudaCu128
    );
    assert_eq!(
        select_semif_torch_wheel("cpu", &mixed, "linux").unwrap(),
        TorchWheel::Cpu
    );
    assert_eq!(
        select_semif_torch_wheel("cuda:1", &mixed, "linux").unwrap(),
        TorchWheel::CudaCu128
    );
    assert!(select_semif_torch_wheel("cuda:0", &mixed, "linux").is_err());
    assert_ne!(TorchWheel::Cpu.identity(), TorchWheel::CudaCu128.identity());
    assert!(select_semif_torch_wheel("mps", &[], "linux").is_err());
    assert_eq!(
        select_semif_torch_wheel("mps", &[], "macos").unwrap(),
        TorchWheel::DefaultHost
    );
    assert_eq!(
        select_semif_torch_wheel("cpu", &[], "macos").unwrap(),
        TorchWheel::DefaultHost
    );
    assert!(select_semif_torch_wheel("cuda", &[], "macos").is_err());
    assert_ne!(
        TorchWheel::DefaultHost.identity(),
        TorchWheel::Cpu.identity()
    );
    let parsed = parse_nvidia_smi_csv("0, 8192, 5.0\n1, 24576, 8.0\n");
    let filtered = filter_cuda_visible_devices(&parsed, Some("1")).unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].index, 1);
    assert_eq!(
        select_semif_torch_wheel("cuda", &filtered, "linux").unwrap(),
        TorchWheel::CudaCu128
    );
    assert!(filter_cuda_visible_devices(&parsed, Some("GPU-uuid")).is_err());
}

#[test]
fn this_host_auto_wheel_is_cpu_and_stable() {
    let visible = probe_visible_gpus().unwrap();
    let first = select_semif_torch_wheel("auto", &visible, "linux").unwrap();
    let second = select_semif_torch_wheel("auto", &visible, "linux").unwrap();
    assert_eq!(first, TorchWheel::Cpu);
    assert_eq!(first.identity(), second.identity());
    assert!(select_semif_torch_wheel("cuda", &visible, "linux").is_err());
}

#[test]
fn env_choice_keeps_the_wheel_after_the_probe_would_change() {
    let config = ModelConfig::default();
    let capable = vec![VisibleGpu {
        index: 0,
        free_bytes: 20 * 1024 * 1024 * 1024,
        compute_major: 8,
    }];
    let chosen = EnvChoice::from_visible(&config, &capable, "linux").unwrap();
    let carried = chosen.environment_key(&config).unwrap();
    assert_eq!(chosen.wheel_identity(), TorchWheel::CudaCu128.identity());
    let dropped = vec![VisibleGpu {
        index: 0,
        free_bytes: 8 * 1024 * 1024 * 1024,
        compute_major: 5,
    }];
    let later = EnvChoice::from_visible(&config, &dropped, "linux").unwrap();
    assert_eq!(later.wheel_identity(), TorchWheel::Cpu.identity());
    assert_ne!(later.environment_key(&config).unwrap(), carried);
    assert_eq!(chosen.environment_key(&config).unwrap(), carried);
    assert_eq!(chosen.wheel_identity(), TorchWheel::CudaCu128.identity());
}

async fn read_http_headers(socket: &mut tokio::net::TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = socket.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
}

#[tokio::test]
async fn jev_rejects_oversize_localhost_bodies() {
    let _lock = env_lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_http_headers(&mut socket).await;
        let declared = 8 * 1024 * 1024 + 1;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n{{\"x\":1}}"
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    let root = workspace(&jev_config(&format!(
        "http://127.0.0.1:{port}/v1/systemone"
    )));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", "test-key-not-logged");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = provider
        .evaluate(vec![state()], vec![choice()], 3_000, cancel_flag())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("8MiB"), "{err}");
    assert!(!err.contains("test-key-not-logged"), "{err}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_http_headers(&mut socket).await;
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await;
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..9 {
            let header = format!("{:X}\r\n", chunk.len());
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            if socket.write_all(&chunk).await.is_err() {
                return;
            }
            if socket.write_all(b"\r\n").await.is_err() {
                return;
            }
        }
    });
    let root = workspace(&jev_config(&format!(
        "http://127.0.0.1:{port}/v1/systemone"
    )));
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = provider
        .evaluate(vec![state()], vec![choice()], 10_000, cancel_flag())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("8MiB"), "{err}");
    assert!(!err.contains("test-key-not-logged"), "{err}");
}

#[tokio::test]
async fn jev_cancel_during_slow_body_does_not_hang() {
    let _lock = env_lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_http_headers(&mut socket).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\nConnection: close\r\n\r\n{")
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let root = workspace(&jev_config(&format!(
        "http://127.0.0.1:{port}/v1/systemone"
    )));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", "test-key-not-logged");
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let cancel = cancel_flag();
    let flag = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        flag.store(true, Ordering::Release);
    });
    let err = tokio::time::timeout(
        Duration::from_secs(3),
        provider.evaluate(vec![state()], vec![choice()], 10_000, cancel),
    )
    .await
    .expect("slow body ignored cancel")
    .unwrap_err()
    .to_string();
    assert!(err.contains("cancel"), "{err}");
    assert!(!err.contains("test-key-not-logged"), "{err}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_http_headers(&mut socket).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\nConnection: close\r\n\r\n{")
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let root = workspace(&jev_config(&format!(
        "http://127.0.0.1:{port}/v1/systemone"
    )));
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = tokio::time::timeout(
        Duration::from_secs(3),
        provider.evaluate(vec![state()], vec![choice()], 200, cancel_flag()),
    )
    .await
    .expect("slow body ignored the deadline")
    .unwrap_err()
    .to_string();
    assert!(err.contains("timed out"), "{err}");
    assert!(!err.contains("test-key-not-logged"), "{err}");
}

#[tokio::test]
async fn jev_clips_long_unicode_errors_without_the_key() {
    let _lock = env_lock().await;
    let secret = "test-key-not-logged";
    let body = format!("{secret}\n{}", "😀".repeat(150));
    let (endpoint, _) = serve(vec![Reply {
        status: 500,
        retry_after: None,
        body,
        delay_ms: 0,
    }])
    .await;
    let root = workspace(&jev_config(&endpoint));
    let _key = EnvVar::set("CHECKWEAVE_TEST_JEV_KEY", secret);
    let mut provider = ModelProvider::new(root.path()).await.unwrap();
    let err = provider
        .evaluate(vec![state()], vec![choice()], 3_000, cancel_flag())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("500"), "{err}");
    assert!(err.contains("[redacted]"), "{err}");
    assert!(err.contains('😀'), "{err}");
    assert!(!err.contains(secret), "{err}");
}
