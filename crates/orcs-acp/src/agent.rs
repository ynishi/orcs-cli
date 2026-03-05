//! ACP Agent trait implementation for ORCS.
//!
//! Bridges ACP protocol requests to the ORCS runtime via [`OrcsApp`]'s
//! existing `run_command` flow.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ContentChunk, Implementation, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, ProtocolVersion, Result,
    SessionId, SessionNotification, SessionUpdate, StopReason,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::convert;

/// ORCS ACP Agent.
///
/// Holds sessions and dispatches ACP requests to ORCS runtime.
/// Each session maps an ACP `SessionId` to an ORCS IO channel pair.
pub struct OrcsAgent {
    /// Connection back to the ACP client for sending notifications.
    client: RefCell<Option<Rc<dyn AcpClient>>>,
    /// Active sessions keyed by ACP session ID.
    sessions: RefCell<HashMap<String, SessionState>>,
    /// Sender for submitting prompt text to the ORCS engine.
    /// Connected to the IOInput channel of the OrcsApp.
    prompt_tx: mpsc::Sender<PromptJob>,
}

/// Abstraction over ACP client connection for sending session notifications.
#[async_trait::async_trait(?Send)]
pub trait AcpClient {
    async fn send_update(
        &self,
        notification: SessionNotification,
    ) -> std::result::Result<(), agent_client_protocol::Error>;
}

/// A prompt job to be processed by the engine.
pub struct PromptJob {
    /// ACP session ID.
    pub session_id: String,
    /// Text extracted from the prompt content blocks.
    pub text: String,
    /// Channel to send the response back.
    pub response_tx: tokio::sync::oneshot::Sender<PromptResponse>,
}

/// Per-session state.
struct SessionState {
    /// Working directory for this session.
    _cwd: std::path::PathBuf,
}

impl OrcsAgent {
    /// Creates a new ORCS ACP agent.
    pub fn new(prompt_tx: mpsc::Sender<PromptJob>) -> Self {
        Self {
            client: RefCell::new(None),
            sessions: RefCell::new(HashMap::new()),
            prompt_tx,
        }
    }

    /// Sets the client connection for sending notifications.
    pub fn set_client(&self, client: Rc<dyn AcpClient>) {
        *self.client.borrow_mut() = Some(client);
    }

    /// Sends a session update notification to the client.
    async fn notify_update(
        &self,
        session_id: &str,
        update: SessionUpdate,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        let client = self.client.borrow().clone();
        if let Some(client) = client.as_ref() {
            let notification = SessionNotification::new(SessionId::new(session_id), update);
            client.send_update(notification).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for OrcsAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse> {
        info!(
            client_version = %args.protocol_version,
            "ACP initialize"
        );

        let response = InitializeResponse::new(ProtocolVersion::LATEST)
            .agent_capabilities(
                AgentCapabilities::new()
                    .load_session(false)
                    .prompt_capabilities(PromptCapabilities::default()),
            )
            .agent_info(
                Implementation::new("orcs", env!("CARGO_PKG_VERSION"))
                    .title("ORCS - Hackable Agentic Shell"),
            );

        Ok(response)
    }

    async fn authenticate(&self, _args: AuthenticateRequest) -> Result<AuthenticateResponse> {
        // ORCS does not require authentication — the agent runs locally.
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse> {
        let session_id = uuid::Uuid::new_v4().to_string();
        info!(session_id = %session_id, cwd = %args.cwd.display(), "ACP new_session");

        self.sessions
            .borrow_mut()
            .insert(session_id.clone(), SessionState { _cwd: args.cwd });

        Ok(NewSessionResponse::new(SessionId::new(session_id)))
    }

    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse> {
        let session_id = args.session_id.0.to_string();
        debug!(session_id = %session_id, "ACP prompt");

        // Verify session exists.
        if !self.sessions.borrow().contains_key(&session_id) {
            return Err(agent_client_protocol::Error::new(
                agent_client_protocol::ErrorCode::InvalidParams.into(),
                format!("unknown session: {session_id}"),
            ));
        }

        // Extract text from content blocks.
        let text = convert::extract_text_from_prompt(&args.prompt);

        if text.is_empty() {
            // Empty prompt → return immediately.
            return Ok(PromptResponse::new(StopReason::EndTurn));
        }

        // Create oneshot for response.
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        let job = PromptJob {
            session_id: session_id.clone(),
            text,
            response_tx,
        };

        // Send job to engine worker.
        self.prompt_tx.send(job).await.map_err(|_| {
            agent_client_protocol::Error::new(
                agent_client_protocol::ErrorCode::InternalError.into(),
                "engine worker closed",
            )
        })?;

        // Send initial "thinking" notification.
        let _ = self
            .notify_update(
                &session_id,
                SessionUpdate::AgentThoughtChunk(ContentChunk::new("Processing...".into())),
            )
            .await;

        // Wait for engine to complete.
        match response_rx.await {
            Ok(response) => Ok(response),
            Err(_) => {
                warn!(session_id = %session_id, "prompt response channel dropped");
                Ok(PromptResponse::new(StopReason::EndTurn))
            }
        }
    }

    async fn cancel(&self, args: CancelNotification) -> Result<()> {
        let session_id = args.session_id.0.to_string();
        info!(session_id = %session_id, "ACP cancel");
        // TODO: signal cancellation to ORCS engine via Veto/Cancel signal.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{ContentBlock, TextContent};

    fn make_agent() -> (OrcsAgent, mpsc::Receiver<PromptJob>) {
        let (tx, rx) = mpsc::channel(16);
        (OrcsAgent::new(tx), rx)
    }

    #[tokio::test]
    async fn initialize_returns_orcs_info() {
        let (agent, _rx) = make_agent();
        let req = InitializeRequest::new(ProtocolVersion::LATEST);
        let resp = agent
            .initialize(req)
            .await
            .expect("initialize should succeed");

        let info = resp.agent_info.expect("agent_info should be set");
        assert_eq!(info.name, "orcs");
        assert!(info
            .title
            .as_deref()
            .expect("title should be set")
            .contains("ORCS"));
    }

    #[tokio::test]
    async fn authenticate_succeeds_without_credentials() {
        let (agent, _rx) = make_agent();
        let req = AuthenticateRequest::new("any-method");
        let _resp = agent
            .authenticate(req)
            .await
            .expect("authenticate should succeed");
    }

    #[tokio::test]
    async fn new_session_returns_unique_ids() {
        let (agent, _rx) = make_agent();
        let req1 = NewSessionRequest::new("/tmp/a");
        let req2 = NewSessionRequest::new("/tmp/b");

        let resp1 = agent
            .new_session(req1)
            .await
            .expect("new_session should succeed");
        let resp2 = agent
            .new_session(req2)
            .await
            .expect("new_session should succeed");

        assert_ne!(resp1.session_id, resp2.session_id);
    }

    #[tokio::test]
    async fn prompt_unknown_session_returns_error() {
        let (agent, _rx) = make_agent();
        let req = PromptRequest::new(
            SessionId::new("nonexistent"),
            vec![ContentBlock::Text(TextContent::new("hello"))],
        );

        let result = agent.prompt(req).await;
        assert!(result.is_err(), "prompt with unknown session should fail");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_dispatches_to_engine() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, mut rx) = make_agent();

                // Create session first.
                let session_resp = agent
                    .new_session(NewSessionRequest::new("/tmp"))
                    .await
                    .expect("new_session should succeed");
                let sid = session_resp.session_id.clone();

                let req = PromptRequest::new(
                    sid,
                    vec![ContentBlock::Text(TextContent::new("test message"))],
                );

                // Spawn the prompt call on LocalSet (OrcsAgent is !Send).
                let prompt_task = tokio::task::spawn_local(async move { agent.prompt(req).await });

                // Receive the dispatched job.
                let job = rx
                    .recv()
                    .await
                    .expect("should receive prompt job from channel");
                assert_eq!(job.text, "test message");

                // Send response back.
                let _ = job
                    .response_tx
                    .send(PromptResponse::new(StopReason::EndTurn));

                let result = prompt_task.await.expect("prompt task should complete");
                assert!(result.is_ok());
            })
            .await;
    }
}
