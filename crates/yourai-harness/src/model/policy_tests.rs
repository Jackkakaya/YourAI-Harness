use super::*;
use std::time::Duration;

fn failure() -> YourAiError {
    ErrorKind::Provider {
        name: "custom-model",
        message: "opaque vendor failure".into(),
    }
    .into()
}
fn request() -> ModelRequest {
    ModelRequest::new(ChatRequest::new(vec![]), ChatOptions::default())
}

#[derive(Clone, Copy)]
enum Mode {
    SetupError,
    RepeatedErrors,
    EndThenError,
    Empty,
}
struct CustomProvider {
    class: ModelErrorClass,
    mode: Mode,
}
impl ModelProvider for CustomProvider {
    fn model_iden(&self) -> &str {
        "custom"
    }
    fn classify_error(&self, _: &YourAiError) -> ModelErrorClass {
        self.class
    }
    fn retry_after(&self, _: &YourAiError) -> Option<Duration> {
        Some(Duration::from_secs(4))
    }
    // Intentionally different from the default mapping to verify wrapper transparency.
    fn recovery(&self, _: &YourAiError) -> ModelRecovery {
        ModelRecovery::Compact
    }
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async { Err(failure()) })
    }
    fn stream_events<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async move {
            let events = match self.mode {
                Mode::SetupError => return Err(failure()),
                Mode::RepeatedErrors => vec![Err(failure()), Err(failure())],
                Mode::EndThenError => vec![
                    Ok(ChatStreamEvent::End(StreamEnd::default())),
                    Err(failure()),
                ],
                Mode::Empty => vec![],
            };
            Ok(Box::pin(futures_util::stream::iter(events)) as ModelEventStream)
        })
    }
}
fn wrapped(class: ModelErrorClass, mode: Mode) -> (Arc<dyn ModelProvider>, Arc<ModelBudget>) {
    let budget = ModelBudget::new();
    let inner = ConfiguredModel::wrap(
        Arc::new(SourceModel {
            inner: Arc::new(CustomProvider { class, mode }),
            source: "test",
        }),
        Some(Duration::from_secs(1)),
        None,
    )
    .unwrap();
    (
        Arc::new(MeteredModel {
            inner,
            budget: budget.clone(),
        }),
        budget,
    )
}

#[tokio::test(start_paused = true)]
async fn custom_non_http_policy_controls_complete_setup_and_stream_failures() {
    for class in [ModelErrorClass::RateLimited, ModelErrorClass::Unclassified] {
        for mode in [Mode::SetupError, Mode::RepeatedErrors] {
            let (model, budget) = wrapped(class, mode);
            assert_eq!(model.classify_error(&failure()), class);
            assert_eq!(model.recovery(&failure()), ModelRecovery::Compact);
            assert_eq!(model.retry_after(&failure()), Some(Duration::from_secs(4)));
            assert!(model.complete(request()).await.is_err());
            assert_eq!(
                budget.control.remaining().as_secs(),
                if class == ModelErrorClass::RateLimited {
                    4
                } else {
                    0
                }
            );
            assert_eq!(budget.snapshot().requests.failed, 1);
            // HTTP-only metrics must not fabricate 429 for a plugin classification.
            assert_eq!(budget.snapshot().requests.rate_limited, 0);

            let (model, budget) = wrapped(class, mode);
            match model.stream_events(request()).await {
                Err(_) => assert!(matches!(mode, Mode::SetupError)),
                Ok(mut stream) => {
                    assert!(stream.next().await.unwrap().is_err());
                }
            }
            assert_eq!(
                budget.control.remaining().as_secs(),
                if class == ModelErrorClass::RateLimited {
                    4
                } else {
                    0
                }
            );
            assert_eq!(budget.snapshot().requests.failed, 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn stream_terminal_accounting_does_not_repeat_or_invent_cooldown() {
    let (model, budget) = wrapped(ModelErrorClass::RateLimited, Mode::RepeatedErrors);
    let mut stream = model.stream_events(request()).await.unwrap();
    assert!(stream.next().await.unwrap().is_err());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(stream.next().await.unwrap().is_err());
    assert_eq!(budget.control.remaining(), Duration::from_secs(2));
    assert_eq!(budget.snapshot().requests.failed, 1);
    drop(stream);
    assert_eq!(budget.snapshot().requests.cancelled, 0);

    let (model, budget) = wrapped(ModelErrorClass::RateLimited, Mode::EndThenError);
    let mut stream = model.stream_events(request()).await.unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_err());
    assert!(budget.control.remaining().is_zero());
    assert_eq!(budget.snapshot().requests.completed, 1);
    assert_eq!(budget.snapshot().requests.failed, 0);

    let (model, budget) = wrapped(ModelErrorClass::RateLimited, Mode::Empty);
    let mut stream = model.stream_events(request()).await.unwrap();
    assert!(stream.next().await.is_none());
    assert!(budget.control.remaining().is_zero());
    assert_eq!(budget.snapshot().requests.failed, 1);

    let (model, budget) = wrapped(ModelErrorClass::RateLimited, Mode::RepeatedErrors);
    drop(model.stream_events(request()).await.unwrap());
    assert!(budget.control.remaining().is_zero());
    assert_eq!(budget.snapshot().requests.cancelled, 1);
}

#[test]
fn unspecified_provider_does_not_inherit_vendor_protocol_policy() {
    struct Unspecified;
    impl ModelProvider for Unspecified {
        fn model_iden(&self) -> &str {
            "unspecified"
        }
        fn complete<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            unreachable!()
        }
        fn stream_events<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            unreachable!()
        }
    }
    let error = ErrorKind::Model {
        source: genai::Error::HttpError {
            status: "429".parse().unwrap(),
            canonical_reason: "Too Many Requests".into(),
            body: "{}".into(),
        },
    }
    .into();
    assert_eq!(
        Unspecified.classify_error(&error),
        ModelErrorClass::Unclassified
    );
    assert_eq!(Unspecified.recovery(&error), ModelRecovery::Fatal);
    assert_eq!(Unspecified.retry_after(&error), None);
}
