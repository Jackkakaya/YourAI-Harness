#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use yourai_core::prelude::*;
    struct Model;
    impl ModelProvider for Model {
        fn model_iden(&self) -> &str {
            "test"
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
            Box::pin(std::future::pending())
        }
    }
    async fn fixture() -> (tempfile::TempDir, crate::Harness) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
        config.system_prompt = Some("test".into());
        let harness = crate::Harness::open(config, Arc::new(Model)).await.unwrap();
        (dir, harness)
    }
    #[tokio::test]
    async fn diagnostic_flush_failure_does_not_swallow_closed_inputs() {
        use futures_util::FutureExt;
        let (_dir, harness) = fixture().await;
        harness
            .host
            .submit_async(In::user_text("kept"))
            .await
            .unwrap();
        let connection =
            rusqlite::Connection::open(harness.host.context().transcript_path.unwrap()).unwrap();
        connection.execute("DROP TABLE model_requests", []).unwrap();
        let metered = crate::MeteredModel {
            inner: Arc::new(Model),
            budget: harness.budget.clone(),
        };
        assert!(metered
            .stream_events(ModelRequest::new(
                ChatRequest::from_user("test"),
                ChatOptions::default()
            ))
            .now_or_never()
            .is_none());
        assert_eq!(harness.close().await.unwrap().len(), 1);
        assert!(harness.budget.snapshot().requests.journal_errors > 0);
        assert!(harness
            .host
            .last_error()
            .unwrap()
            .contains("model_requests"));
        assert!(harness.close().await.unwrap().is_empty());
    }
}
