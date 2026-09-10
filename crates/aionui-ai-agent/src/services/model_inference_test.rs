use super::*;
use aion_types::message::TokenUsage;
use aionui_db::{CreateProviderParams, SqliteProviderRepository, init_database_memory};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

struct StalledProviderServer {
    url: String,
    started: mpsc::UnboundedReceiver<()>,
    closed: mpsc::UnboundedReceiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StalledProviderServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn stalled_provider_server(prefix: &'static str) -> StalledProviderServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (started_tx, started) = mpsc::unbounded_channel();
    let (closed_tx, closed) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let started_tx = started_tx.clone();
            let closed_tx = closed_tx.clone();
            connections.spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let body_len: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if request.len() >= header_end + 4 + body_len {
                            break;
                        }
                    }
                }
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                    )
                    .await
                    .unwrap();
                if !prefix.is_empty() {
                    socket
                        .write_all(format!("{:x}\r\n{prefix}\r\n", prefix.len()).as_bytes())
                        .await
                        .unwrap();
                }
                started_tx.send(()).unwrap();
                // Headers complete, but the provider never emits an SSE frame.
                let count = socket.read(&mut buffer).await;
                assert!(
                    matches!(count, Ok(0) | Err(_)),
                    "unexpected second request on the stalled socket"
                );
                let _ = closed_tx.send(());
            });
        }
    });
    StalledProviderServer {
        url,
        started,
        closed,
        task,
    }
}

async fn configure_server(repo: &Arc<dyn IProviderRepository>, server: &StalledProviderServer) {
    repo.update(
        "system_default_user",
        "provider",
        aionui_db::UpdateProviderParams {
            base_url: Some(&server.url),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_callers_keep_all_four_slots_until_provider_cleanup() {
    let (mut service, repo) = service().await;
    service.inference_timeout = Duration::from_secs(1);
    let service = Arc::new(service);
    let mut server = stalled_provider_server("").await;
    configure_server(&repo, &server).await;
    let mut calls = Vec::new();
    for _ in 0..4 {
        let service = service.clone();
        calls.push(tokio::spawn(async move {
            service.infer("system_default_user", selected(), "data".into()).await
        }));
    }
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_secs(5), server.started.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(matches!(
        service.infer("system_default_user", selected(), "data".into()).await,
        Err(AgentError::RateLimited)
    ));
    for call in calls {
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
    }
    let fifth = tokio::time::timeout(
        Duration::from_millis(200),
        service.infer("system_default_user", selected(), "data".into()),
    )
    .await;
    assert!(
        matches!(fifth, Ok(Err(AgentError::RateLimited))),
        "cancelled HTTP callers must not release active provider slots: {fifth:?}"
    );
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_secs(3), server.closed.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match service.infer("system_default_user", selected(), "data".into()).await {
                Err(AgentError::RateLimited) => tokio::task::yield_now().await,
                result => break result,
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        result.status,
        ModelInferenceStatus::Timeout,
        "a slot must be available after provider cleanup"
    );
    tokio::time::timeout(Duration::from_secs(3), server.closed.recv())
        .await
        .unwrap()
        .unwrap();
}

#[derive(Clone)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn provider_retry_warning_never_exposes_sensitive_response_text() {
    let (service, repo) = service().await;
    let mut server = stalled_provider_server(
        "data: {\"error\":{\"code\":500,\"message\":\"INFERENCE_PRIVATE_RESPONSE_SENTINEL\"}}\n\n",
    )
    .await;
    configure_server(&repo, &server).await;
    let row = repo
        .find_by_id("system_default_user", "provider")
        .await
        .unwrap()
        .unwrap();
    let config = service.config(&row, "selected-model").unwrap();
    let captured = CapturedLog(Arc::new(Mutex::new(Vec::new())));
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let result = tokio::task::spawn_blocking(move || {
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("inference capture control");
            infer_in_runtime(
                config,
                "selected-model".into(),
                "data".into(),
                Duration::from_millis(200),
            )
        })
    })
    .await
    .unwrap();
    assert_eq!(
        result.0,
        ModelInferenceStatus::Timeout,
        "the retry remains pending until the inference deadline"
    );
    tokio::time::timeout(Duration::from_secs(3), server.started.recv())
        .await
        .unwrap()
        .unwrap();
    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("inference capture control"),
        "the warning observer must be active"
    );
    assert!(
        !log.contains("INFERENCE_PRIVATE_RESPONSE_SENTINEL"),
        "provider response leaked into logs: {log}"
    );
}

#[tokio::test]
async fn timeout_closes_the_detached_provider_stream() {
    let (mut service, repo) = service().await;
    service.inference_timeout = Duration::from_millis(500);
    let mut server = stalled_provider_server("").await;
    configure_server(&repo, &server).await;
    let call = tokio::spawn(async move { service.infer("system_default_user", selected(), "data".into()).await });
    tokio::time::timeout(Duration::from_secs(3), server.started.recv())
        .await
        .unwrap()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), call)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.status, ModelInferenceStatus::Timeout);
    assert_eq!(result.answer, None);
    tokio::time::timeout(Duration::from_secs(3), server.closed.recv())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn isolated_provider_runtime_preserves_successful_answers() {
    let (service, repo) = service().await;
    let mut server = stalled_provider_server("data: {\"choices\":[{\"delta\":{\"content\":\"bounded advice\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await;
    configure_server(&repo, &server).await;
    let result = service
        .infer("system_default_user", selected(), "data".into())
        .await
        .unwrap();
    assert_eq!(result.status, ModelInferenceStatus::Ok);
    assert_eq!(result.answer.as_deref(), Some("bounded advice"));
    assert_eq!(result.model, "selected-model");
    tokio::time::timeout(Duration::from_secs(3), server.closed.recv())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn output_limit_accepts_64kib_and_rejects_multibyte_overflow_without_partial_text() {
    let expected = "x".repeat(65_536);
    let ((status, answer), _) = run(vec![LlmEvent::TextDelta(expected.clone()), done(StopReason::EndTurn)]).await;
    assert_eq!(status, ModelInferenceStatus::Ok);
    assert_eq!(answer.as_deref(), Some(expected.as_str()));
    let ((status, answer), _) = run(vec![
        LlmEvent::TextDelta("x".repeat(65_535)),
        LlmEvent::TextDelta("é".into()),
        done(StopReason::EndTurn),
    ])
    .await;
    assert_eq!(status, ModelInferenceStatus::Failed);
    assert_eq!(answer, None);
}

struct FakeProvider {
    events: Vec<LlmEvent>,
    request: Arc<Mutex<Option<LlmRequest>>>,
}

#[async_trait::async_trait]
impl LlmProvider for FakeProvider {
    async fn stream(&self, request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, aion_providers::ProviderError> {
        *self.request.lock().unwrap() = Some(request.clone());
        let (tx, rx) = mpsc::channel(self.events.len().max(1));
        for event in &self.events {
            tx.send(event.clone()).await.unwrap();
        }
        Ok(rx)
    }
}

async fn run(events: Vec<LlmEvent>) -> ((ModelInferenceStatus, Option<String>), LlmRequest) {
    let request = Arc::new(Mutex::new(None));
    let result = infer_text(
        Arc::new(FakeProvider {
            events,
            request: request.clone(),
        }),
        "selected-model",
        "Analyze this supplied snapshot".into(),
    )
    .await;
    let captured = request.lock().unwrap().take().unwrap();
    (result, captured)
}

fn done(reason: StopReason) -> LlmEvent {
    LlmEvent::Done {
        stop_reason: reason,
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn streams_selected_model_without_tools_or_chat_history() {
    let ((status, answer), request) = run(vec![
        LlmEvent::TextDelta("{\"sku\":\"".into()),
        LlmEvent::TextDelta("check demand\"}".into()),
        done(StopReason::EndTurn),
    ])
    .await;
    assert_eq!(status, ModelInferenceStatus::Ok);
    assert_eq!(answer.as_deref(), Some("{\"sku\":\"check demand\"}"));
    assert_eq!(request.model, "selected-model");
    assert!(request.tools.is_empty());
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.messages[0].role, Role::User);
}

#[tokio::test]
async fn rejects_tools_errors_truncation_and_unfinished_streams() {
    let cases = vec![
        (vec![done(StopReason::EndTurn)], ModelInferenceStatus::NoAnswer),
        (
            vec![LlmEvent::TextDelta("partial".into())],
            ModelInferenceStatus::Failed,
        ),
        (
            vec![LlmEvent::Error("secret raw vendor body".into())],
            ModelInferenceStatus::Failed,
        ),
        (
            vec![LlmEvent::TextDelta("partial".into()), done(StopReason::MaxTokens)],
            ModelInferenceStatus::Failed,
        ),
        (
            vec![LlmEvent::ToolUse {
                id: "t".into(),
                name: "approve".into(),
                input: serde_json::json!({}),
                extra: None,
            }],
            ModelInferenceStatus::ToolsRequired,
        ),
    ];
    for (events, expected) in cases {
        let ((status, answer), _) = run(events).await;
        assert_eq!(status, expected);
        assert_eq!(answer, None);
    }
}

#[test]
fn validates_empty_and_oversized_questions() {
    assert!(
        matches!(validate_question(" "), Err(AgentError::BadRequest(reason)) if reason == "MODEL_INFERENCE_INVALID_QUESTION")
    );
    assert!(
        matches!(validate_question(&"x".repeat(MAX_MODEL_INFERENCE_BYTES + 1)), Err(AgentError::BadRequest(reason)) if reason == "MODEL_INFERENCE_INVALID_QUESTION")
    );
    validate_question("supplied data").unwrap();
}

async fn service() -> (ProviderModelInference, Arc<dyn IProviderRepository>) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let secret = aionui_common::encrypt_string("fake-provider-key", &[0xAB; 32]).unwrap();
    repo.create(CreateProviderParams {
        id: Some("provider"),
        user_id: "system_default_user",
        platform: "openai",
        name: "Mock API",
        base_url: "http://127.0.0.1:1/v1",
        api_key_encrypted: &secret,
        models: "[\"selected-model\"]",
        enabled: true,
        capabilities: "[]",
        context_limit: None,
        model_protocols: None,
        model_enabled: None,
        model_health: None,
        model_settings: "{}",
        bedrock_config: None,
        is_full_url: false,
    })
    .await
    .unwrap();
    (
        ProviderModelInference::new(repo.clone(), [0xAB; 32], PathBuf::from("/tmp/model-inference-test")),
        repo,
    )
}

fn selected() -> ProviderWithModel {
    ProviderWithModel {
        provider_id: "provider".into(),
        model: "initial-model".into(),
        use_model: Some("selected-model".into()),
    }
}

#[tokio::test]
async fn default_model_is_user_scoped_and_skips_disabled_models_without_a_conversation() {
    let (service, repo) = service().await;
    assert_eq!(
        service.default_model("system_default_user").await.unwrap().model,
        "selected-model"
    );
    assert!(
        matches!(service.default_model("other-user").await, Err(AgentError::BadRequest(reason)) if reason == "MODEL_NOT_SELECTED")
    );
    repo.update(
        "system_default_user",
        "provider",
        aionui_db::UpdateProviderParams {
            models: Some("[\"disabled\",\"selected-model\"]"),
            model_enabled: Some(Some("{\"disabled\":false}")),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        service.default_model("system_default_user").await.unwrap().model,
        "selected-model"
    );
    repo.update(
        "system_default_user",
        "provider",
        aionui_db::UpdateProviderParams {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(service.default_model("system_default_user").await, Err(AgentError::BadRequest(reason)) if reason == "MODEL_NOT_SELECTED")
    );
}

#[tokio::test]
async fn enforces_provider_owner_and_enabled_state_before_network() {
    let (service, repo) = service().await;
    assert!(
        matches!(service.infer("different-user", selected(), "data".into()).await, Err(AgentError::NotFound(reason)) if reason == "MODEL_PROVIDER_NOT_FOUND")
    );
    repo.update(
        "system_default_user",
        "provider",
        aionui_db::UpdateProviderParams {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(service.infer("system_default_user", selected(), "data".into()).await, Err(AgentError::Forbidden(reason)) if reason == "MODEL_DISABLED")
    );
}

#[tokio::test]
async fn reuses_saved_credentials_url_model_and_protocol_overrides() {
    let (service, repo) = service().await;
    let mut row = repo
        .find_by_id("system_default_user", "provider")
        .await
        .unwrap()
        .unwrap();
    row.model_settings = r#"{"selected-model":{"openai_api_mode":"chat_completions"}}"#.into();
    let config = service.config(&row, "selected-model").unwrap();
    assert_eq!(config.api_key, "fake-provider-key");
    assert_eq!(config.model, "selected-model");
    assert!(config.base_url.starts_with("http://127.0.0.1:1"));
    assert!(!config.session.enabled);
    assert!(config.mcp.servers.is_empty());
}
