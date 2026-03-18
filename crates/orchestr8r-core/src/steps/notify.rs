use super::StepExecutor;
use crate::types::{StepContext, StepDef, StepError, StepOutput, StepType};
use async_trait::async_trait;
use orchestr8r_notify::Notification;

pub struct NotifyExecutor;

#[async_trait]
impl StepExecutor for NotifyExecutor {
    fn step_type(&self) -> StepType {
        StepType::Notify
    }

    async fn execute(&self, step_def: &StepDef, ctx: &StepContext) -> Result<StepOutput, StepError> {
        let body = step_def.message.clone().unwrap_or_default();
        ctx.notifier
            .send(&Notification {
                summary: "orchestr8r".to_string(),
                body: body.clone(),
            })
            .await
            .map_err(|e| StepError::ExecutionFailed(e.to_string()))?;
        Ok(StepOutput {
            stdout: body,
            stderr: String::new(),
            exit_code: Some(0),
            accepted: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StepDef, StepType};
    use orchestr8r_notify::{Notification, NotifyError, Notifier};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    struct MockNotifier {
        bodies: Mutex<Vec<String>>,
    }

    impl MockNotifier {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                bodies: Mutex::new(Vec::new()),
            })
        }

        fn bodies(&self) -> Vec<String> {
            self.bodies.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Notifier for MockNotifier {
        fn name(&self) -> &str {
            "mock"
        }
        async fn send(&self, n: &Notification) -> Result<(), NotifyError> {
            self.bodies.lock().unwrap().push(n.body.clone());
            Ok(())
        }
    }

    fn make_ctx(notifier: Arc<dyn Notifier>) -> StepContext {
        StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".to_string(),
            iteration: 0,
            step_index: 0,
            scratch_dir: std::env::temp_dir(),
            workspace_dir: None,
            checkpoint_tx: None,
            session_manager: None,
            notifier,
            log_fn: None,
            progress_fn: None,
        }
    }

    #[tokio::test]
    async fn notify_executor_sends_step_message() {
        // GIVEN
        let notifier = MockNotifier::new();
        let ctx = make_ctx(notifier.clone());
        let step = StepDef {
            step_type: StepType::Notify,
            command: None,
            message: Some("deployment complete".to_string()),
            session: None,
            notify: None,
            agent: Default::default(),
        };

        // WHEN
        let output = NotifyExecutor.execute(&step, &ctx).await.unwrap();

        // THEN
        assert_eq!(notifier.bodies(), vec!["deployment complete"]);
        assert_eq!(output.stdout, "deployment complete");
        assert_eq!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn notify_executor_empty_message_sends_empty_body() {
        // GIVEN
        let notifier = MockNotifier::new();
        let ctx = make_ctx(notifier.clone());
        let step = StepDef {
            step_type: StepType::Notify,
            command: None,
            message: None,
            session: None,
            notify: None,
            agent: Default::default(),
        };

        // WHEN
        NotifyExecutor.execute(&step, &ctx).await.unwrap();

        // THEN
        assert_eq!(notifier.bodies(), vec![""]);
    }
}
