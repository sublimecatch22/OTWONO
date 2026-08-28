//! The shipped adapters, exercised against a server that speaks the real
//! protocols. No mock provider exists inside the application.

mod support;

use futures_util::StreamExt;
use support::{Behaviour, FakeRuntime};

use otwono_providers::{adapter_for, ChatDelta, ChatRequest, ChatTurn, ProviderError};
use otwono_types::provider::{CapabilitySource, ConnectionHealth, ProviderKind};

async fn collect(stream: otwono_providers::ChatStream) -> (String, Option<ChatDelta>) {
    let mut text = String::new();
    let mut done = None;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item.expect("stream item") {
            ChatDelta::Text(chunk) => text.push_str(&chunk),
            terminal @ ChatDelta::Done { .. } => {
                done = Some(terminal);
                break;
            }
        }
    }
    (text, done)
}

// ------------------------------------------------------------------ Ollama

#[tokio::test]
async fn ollama_reports_a_healthy_connection_and_lists_models() {
    let runtime = FakeRuntime::start(Behaviour::ollama_default()).await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);

    let test = provider.test().await;
    assert_eq!(test.health, ConnectionHealth::Reachable);
    assert_eq!(test.models.len(), 2);
    assert!(test.detail.contains("2 models"), "{}", test.detail);
    assert!(test.latency_ms.is_some());
}

#[tokio::test]
async fn ollama_capabilities_come_from_the_runtime_when_it_reports_them() {
    let runtime = FakeRuntime::start(Behaviour::ollama_default()).await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);

    let models = provider.list_models().await.unwrap();
    let chat = models
        .iter()
        .find(|m| m.id.starts_with("llama3.1"))
        .unwrap();
    assert_eq!(chat.capability_source, CapabilitySource::Reported);
    assert!(chat.capabilities.chat);
    assert!(chat.capabilities.tool_calling, "the runtime reported tools");
    assert!(
        !chat.capabilities.vision,
        "the runtime did not report vision"
    );
    assert_eq!(chat.capabilities.context_length, Some(131_072));
    assert_eq!(chat.parameter_size.as_deref(), Some("8B"));
}

#[tokio::test]
async fn ollama_falls_back_to_inference_when_the_runtime_reports_nothing() {
    let runtime = FakeRuntime::start(Behaviour {
        reported_capabilities: vec![],
        ..Behaviour::ollama_default()
    })
    .await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);

    let models = provider.list_models().await.unwrap();
    let embed = models
        .iter()
        .find(|m| m.id.starts_with("nomic-embed"))
        .unwrap();
    assert_eq!(embed.capability_source, CapabilitySource::Inferred);
    assert!(embed.capabilities.embeddings);
    assert!(
        !embed.capabilities.chat,
        "an embedding model is not a chat model"
    );
    // The context length still came from the runtime even when capabilities
    // did not.
    assert_eq!(embed.capabilities.context_length, Some(131_072));
}

#[tokio::test]
async fn ollama_streams_a_reply_in_order_and_reports_tokens() {
    let runtime = FakeRuntime::start(Behaviour::ollama_default()).await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);

    let stream = provider
        .chat_stream(ChatRequest::new(
            "llama3.1:8b",
            vec![ChatTurn::user("Say hello")],
        ))
        .await
        .unwrap();

    let (text, done) = collect(stream).await;
    assert_eq!(text, "Hello, world");
    match done.expect("a terminal frame") {
        ChatDelta::Done {
            finish_reason,
            token_estimate,
        } => {
            assert_eq!(finish_reason, "stop");
            assert_eq!(
                token_estimate,
                Some(21),
                "prompt and completion tokens are summed"
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert!(runtime.requested().contains(&"/api/chat".to_string()));
}

#[tokio::test]
async fn ollama_reports_an_unknown_model_as_such_rather_than_as_a_generic_failure() {
    let runtime = FakeRuntime::start(Behaviour::ollama_default()).await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);

    let error = match provider
        .chat_stream(ChatRequest::new(
            "not-installed",
            vec![ChatTurn::user("hi")],
        ))
        .await
    {
        Ok(_) => panic!("an unknown model should not open a stream"),
        Err(error) => error,
    };
    let provider_error = error
        .downcast_ref::<ProviderError>()
        .expect("a typed error");
    assert!(
        matches!(provider_error, ProviderError::ModelNotFound { .. }),
        "{provider_error}"
    );
    assert!(
        !provider_error.is_retryable(),
        "retrying will not install the model"
    );
}

#[tokio::test]
async fn a_corrupt_stream_surfaces_an_error_rather_than_silently_truncating() {
    let runtime = FakeRuntime::start(Behaviour {
        corrupt_stream: true,
        ..Behaviour::ollama_default()
    })
    .await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);

    let mut stream = provider
        .chat_stream(ChatRequest::new("llama3.1:8b", vec![ChatTurn::user("hi")]))
        .await
        .unwrap();

    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        if item.is_err() {
            saw_error = true;
            break;
        }
    }
    assert!(
        saw_error,
        "a malformed frame must be reported, not swallowed"
    );
}

#[tokio::test]
async fn ollama_embeddings_round_trip_and_a_refusal_is_reported_as_unsupported() {
    let runtime = FakeRuntime::start(Behaviour::ollama_default()).await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);

    let vectors = provider
        .embed(
            "nomic-embed-text:latest",
            &["hello".to_string(), "world".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].len(), 8);
    assert_ne!(
        vectors[0], vectors[1],
        "different text must embed differently"
    );

    runtime.set(|b| b.embeddings_fail = true);
    let error = provider
        .embed("llama3.1:8b", &["hello".to_string()])
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ProviderError>(),
        Some(ProviderError::Unsupported { .. })
    ));
}

#[tokio::test]
async fn an_unreachable_ollama_is_a_result_not_a_crash() {
    let provider = adapter_for(ProviderKind::Ollama, "http://127.0.0.1:1", None);
    let test = provider.test().await;
    assert_eq!(test.health, ConnectionHealth::Unreachable);
    assert!(test.models.is_empty());
    assert!(test.detail.contains("Start Ollama"), "{}", test.detail);
}

#[tokio::test]
async fn ollama_with_no_models_says_how_to_install_one() {
    let runtime = FakeRuntime::start(Behaviour {
        models: vec![],
        ..Behaviour::ollama_default()
    })
    .await;
    let provider = adapter_for(ProviderKind::Ollama, &runtime.base_url, None);
    let test = provider.test().await;
    assert_eq!(test.health, ConnectionHealth::Reachable);
    assert!(test.detail.contains("ollama pull"), "{}", test.detail);
}

// -------------------------------------------------------- OpenAI-compatible

#[tokio::test]
async fn lm_studio_lists_models_and_labels_capabilities_as_inferred() {
    let runtime = FakeRuntime::start(Behaviour::openai_default()).await;
    let provider = adapter_for(ProviderKind::LmStudio, &runtime.base_url, None);

    let test = provider.test().await;
    assert_eq!(test.health, ConnectionHealth::Reachable);
    assert_eq!(test.models.len(), 2);
    assert!(
        test.detail.contains("inferred from model names"),
        "the UI must not imply these were reported: {}",
        test.detail
    );
    let chat = test.models.iter().find(|m| m.id == "local-model").unwrap();
    assert_eq!(chat.capability_source, CapabilitySource::Inferred);
    assert!(chat.capabilities.chat && chat.capabilities.streaming);
    assert!(
        !chat.capabilities.tool_calling,
        "nothing proved tool support"
    );
}

#[tokio::test]
async fn an_embedding_model_is_probed_rather_than_assumed() {
    let runtime = FakeRuntime::start(Behaviour::openai_default()).await;
    let provider = adapter_for(ProviderKind::LmStudio, &runtime.base_url, None);

    let test = provider.test().await;
    let embed = test
        .models
        .iter()
        .find(|m| m.id == "nomic-embed-text")
        .unwrap();
    assert_eq!(embed.capability_source, CapabilitySource::Probed);
    assert!(embed.capabilities.embeddings);
    assert!(
        runtime.requested().contains(&"/v1/embeddings".to_string()),
        "the probe should actually have been made"
    );
}

#[tokio::test]
async fn a_failing_embedding_probe_means_the_capability_is_not_claimed() {
    let runtime = FakeRuntime::start(Behaviour {
        embeddings_fail: true,
        ..Behaviour::openai_default()
    })
    .await;
    let provider = adapter_for(ProviderKind::LmStudio, &runtime.base_url, None);

    let test = provider.test().await;
    let embed = test
        .models
        .iter()
        .find(|m| m.id == "nomic-embed-text")
        .unwrap();
    assert!(
        !embed.capabilities.embeddings,
        "a failed probe must clear the capability, not leave the guess in place"
    );
}

#[tokio::test]
async fn the_openai_stream_parser_handles_keep_alives_and_the_done_sentinel() {
    let runtime = FakeRuntime::start(Behaviour::openai_default()).await;
    let provider = adapter_for(ProviderKind::LmStudio, &runtime.base_url, None);

    let stream = provider
        .chat_stream(ChatRequest {
            temperature: Some(0.2),
            max_output_tokens: Some(256),
            stop: vec!["END".into()],
            ..ChatRequest::new(
                "local-model",
                vec![ChatTurn::system("Be brief"), ChatTurn::user("Hi")],
            )
        })
        .await
        .unwrap();

    let (text, done) = collect(stream).await;
    assert_eq!(text, "Hello, world");
    match done.expect("a terminal frame") {
        ChatDelta::Done {
            finish_reason,
            token_estimate,
        } => {
            assert_eq!(finish_reason, "stop");
            assert_eq!(token_estimate, Some(21));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn an_endpoint_that_needs_a_key_says_so_instead_of_looking_offline() {
    let runtime = FakeRuntime::start(Behaviour {
        require_auth: true,
        ..Behaviour::openai_default()
    })
    .await;
    let provider = adapter_for(ProviderKind::OpenAiCompatible, &runtime.base_url, None);

    let test = provider.test().await;
    assert_eq!(test.health, ConnectionHealth::AuthenticationRequired);
    assert!(test.detail.contains("needs an API key"), "{}", test.detail);
    assert!(
        test.detail.contains("credential manager"),
        "the user should be told where the key goes: {}",
        test.detail
    );
}

#[tokio::test]
async fn supplying_the_key_makes_the_same_endpoint_work() {
    let runtime = FakeRuntime::start(Behaviour {
        require_auth: true,
        ..Behaviour::openai_default()
    })
    .await;
    let provider = adapter_for(
        ProviderKind::OpenAiCompatible,
        &runtime.base_url,
        Some("test-key-value".into()),
    );

    let test = provider.test().await;
    assert_eq!(test.health, ConnectionHealth::Reachable);
    assert_eq!(test.models.len(), 2);
}

#[tokio::test]
async fn an_upstream_failure_is_reported_as_retryable() {
    let runtime = FakeRuntime::start(Behaviour {
        chat_status: Some(503),
        ..Behaviour::openai_default()
    })
    .await;
    let provider = adapter_for(ProviderKind::LmStudio, &runtime.base_url, None);

    let error = match provider
        .chat_stream(ChatRequest::new("local-model", vec![ChatTurn::user("hi")]))
        .await
    {
        Ok(_) => panic!("a 503 should not open a stream"),
        Err(error) => error,
    };
    let provider_error = error.downcast_ref::<ProviderError>().unwrap();
    assert!(matches!(
        provider_error,
        ProviderError::Upstream { status: 503, .. }
    ));
    assert!(provider_error.is_retryable());
}

#[tokio::test]
async fn openai_embeddings_return_one_vector_per_input() {
    let runtime = FakeRuntime::start(Behaviour::openai_default()).await;
    let provider = adapter_for(ProviderKind::LmStudio, &runtime.base_url, None);

    let vectors = provider
        .embed(
            "nomic-embed-text",
            &["one".into(), "two".into(), "three".into()],
        )
        .await
        .unwrap();
    assert_eq!(vectors.len(), 3);
    assert!(vectors.iter().all(|v| v.len() == 8));
}

#[tokio::test]
async fn cancelling_a_stream_stops_reading_without_an_error() {
    let runtime = FakeRuntime::start(Behaviour {
        chat_deltas: (0..50).map(|i| format!("chunk{i} ")).collect(),
        ..Behaviour::openai_default()
    })
    .await;
    let provider = adapter_for(ProviderKind::LmStudio, &runtime.base_url, None);

    let mut stream = provider
        .chat_stream(ChatRequest::new("local-model", vec![ChatTurn::user("hi")]))
        .await
        .unwrap();

    let mut received = 0;
    while let Some(item) = stream.next().await {
        assert!(item.is_ok());
        received += 1;
        if received == 3 {
            break;
        }
    }
    // Dropping mid-stream is how "Stop generating" works; it must not panic.
    drop(stream);
    assert_eq!(received, 3);
}
