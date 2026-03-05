//! ACP stdio server entry point.
//!
//! Starts the ACP server on stdin/stdout using `AgentSideConnection`
//! from the `agent-client-protocol` crate.
//!
//! The engine worker bridges ACP prompt requests to the ORCS engine:
//!
//! ```text
//! ACP Client ─── AgentSideConnection ─── OrcsAgent
//!                                           │
//!                                      PromptJob channel
//!                                           │
//!                                      engine_worker
//!                                      ┌────┴────┐
//!                                 IOInputHandle  IOOutputHandle
//!                                      │              │
//!                                      └──── ORCS ────┘
//!                                           Engine
//! ```

use std::rc::Rc;

use agent_client_protocol::{
    AgentSideConnection, Client, PromptResponse, SessionNotification, StopReason,
};
use orcs_runtime::io::{IOInputHandle, IOOutputHandle, InputContext};
use orcs_runtime::IOOutput;
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tracing::{debug, error, info, warn};

use crate::agent::{AcpClient, OrcsAgent, PromptJob};
use crate::convert;

/// Wrapper that implements `AcpClient` via `AgentSideConnection`.
struct ConnectionClient {
    conn: Rc<AgentSideConnection>,
}

#[async_trait::async_trait(?Send)]
impl AcpClient for ConnectionClient {
    async fn send_update(
        &self,
        notification: SessionNotification,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        self.conn.session_notification(notification).await
    }
}

/// Idle timeout after first output received (stream complete heuristic).
const IDLE_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(3);
/// Maximum wait for the first output from the engine.
const MAX_WAIT: tokio::time::Duration = tokio::time::Duration::from_secs(120);

/// Checks if the prompt text is a HIL response (approve/reject).
///
/// Returns `true` for "y", "yes", "n", "no" (case-insensitive).
fn is_hil_response(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "y" | "yes" | "n" | "no"
    )
}

/// Processes prompt jobs by sending text to the ORCS engine and
/// streaming output back as ACP session updates.
///
/// For each prompt job:
/// 1. Sends `IOInput::line(text)` to the engine via `IOInputHandle`
/// 2. Collects `IOOutput` from `IOOutputHandle` with idle-timeout
/// 3. Converts each `IOOutput` to an ACP `SessionUpdate` and sends it
/// 4. Completes the prompt when `IOOutput::Prompt` is received or timeout
///
/// HIL (Human-in-the-Loop) approval flow:
/// - When `ShowApprovalRequest` is received, the approval ID is stored
/// - If the next prompt is "y"/"n", it is sent with the stored approval ID
///   as `InputContext`, so the engine's IOBridge can dispatch the correct signal
async fn engine_worker(
    mut rx: mpsc::Receiver<PromptJob>,
    conn: Rc<AgentSideConnection>,
    io_input: IOInputHandle,
    mut io_output: IOOutputHandle,
) {
    // Tracks the most recent pending approval ID across prompts.
    let mut pending_approval_id: Option<String> = None;

    while let Some(job) = rx.recv().await {
        let session_id = &job.session_id;
        debug!(session_id = %session_id, text = %job.text, "engine_worker: processing prompt");

        // Determine if this prompt is a HIL response to a pending approval.
        let io_input_msg = if is_hil_response(&job.text) {
            if let Some(ref approval_id) = pending_approval_id {
                debug!(
                    session_id = %session_id,
                    approval_id = %approval_id,
                    response = %job.text,
                    "engine_worker: HIL response with approval context"
                );
                orcs_runtime::io::IOInput::line_with_context(
                    &job.text,
                    InputContext::with_approval_id(approval_id.as_str()),
                )
            } else {
                orcs_runtime::io::IOInput::line(&job.text)
            }
        } else {
            orcs_runtime::io::IOInput::line(&job.text)
        };

        // Send input to ORCS engine.
        if let Err(e) = io_input.send(io_input_msg).await {
            error!(session_id = %session_id, error = %e, "failed to send input to engine");
            let _ = job
                .response_tx
                .send(PromptResponse::new(StopReason::EndTurn));
            continue;
        }

        // Clear pending approval after sending a HIL response.
        if is_hil_response(&job.text) && pending_approval_id.is_some() {
            pending_approval_id = None;
        }

        // Collect output with idle-timeout detection.
        //   - Before first output: wait up to MAX_WAIT (LLM may be slow to start).
        //   - After first output:  wait up to IDLE_TIMEOUT for more (stream complete).
        let start = tokio::time::Instant::now();
        let mut received_output = false;

        loop {
            if start.elapsed() > MAX_WAIT {
                warn!(
                    session_id = %session_id,
                    "engine_worker: max timeout reached ({}s)",
                    MAX_WAIT.as_secs()
                );
                break;
            }

            let timeout = if received_output {
                IDLE_TIMEOUT
            } else {
                MAX_WAIT
            };

            tokio::select! {
                Some(io_out) = io_output.recv() => {
                    // Prompt signals engine is ready for next input → job done.
                    if matches!(io_out, IOOutput::Prompt { .. }) {
                        debug!(session_id = %session_id, "engine_worker: prompt received, completing");
                        break;
                    }

                    received_output = true;

                    // Track pending approval for HIL flow.
                    if let IOOutput::ShowApprovalRequest { ref id, .. } = io_out {
                        debug!(
                            session_id = %session_id,
                            approval_id = %id,
                            "engine_worker: tracking pending approval"
                        );
                        pending_approval_id = Some(id.clone());
                    }

                    // Convert IOOutput to ACP SessionUpdate.
                    if let Some(update) = convert::io_output_to_session_update(&io_out) {
                        let notification = SessionNotification::new(
                            agent_client_protocol::SessionId::new(session_id.clone()),
                            update,
                        );

                        if let Err(e) = conn.session_notification(notification).await {
                            error!(
                                session_id = %session_id,
                                error = %e,
                                "failed to send session update"
                            );
                        }
                    }
                }
                _ = tokio::time::sleep(timeout) => {
                    debug!(
                        session_id = %session_id,
                        received_output,
                        "engine_worker: timeout, completing"
                    );
                    break;
                }
            }
        }

        // Complete the prompt.
        let _ = job
            .response_tx
            .send(PromptResponse::new(StopReason::EndTurn));
    }
}

/// Runs the ACP server on stdin/stdout.
///
/// This is the main entry point for `orcs acp` mode.
/// It creates an `AgentSideConnection`, wires up the ORCS agent,
/// and runs the JSON-RPC event loop until the connection closes.
///
/// # Arguments
///
/// * `engine` - The ORCS engine (must be started before calling this)
/// * `io_input` - Handle to send input to the engine
/// * `io_output` - Handle to receive output from the engine
///
/// # Errors
///
/// Returns error if the server encounters a fatal I/O error.
pub async fn run_acp_server(
    mut engine: orcs_runtime::OrcsEngine,
    io_input: IOInputHandle,
    io_output: IOOutputHandle,
) -> anyhow::Result<()> {
    info!("Starting ORCS ACP server on stdio");

    // Start the engine so runners begin processing.
    engine.start();

    let local = LocalSet::new();

    let result = local
        .run_until(async {
            // Channel for prompt jobs between agent and engine worker.
            let (prompt_tx, prompt_rx) = mpsc::channel::<PromptJob>(32);

            // Create the ORCS ACP agent.
            let agent = Rc::new(OrcsAgent::new(prompt_tx));

            // Create the stdio connection.
            // stdin = incoming from client, stdout = outgoing to client.
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();

            // AgentSideConnection requires AsyncRead/AsyncWrite (futures traits).
            // tokio::io types implement tokio's traits, so we use compat.
            let stdin_compat = tokio_util::compat::TokioAsyncReadCompatExt::compat(stdin);
            let stdout_compat = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(stdout);

            let (conn, io_task) =
                AgentSideConnection::new(Rc::clone(&agent), stdout_compat, stdin_compat, |fut| {
                    tokio::task::spawn_local(fut);
                });

            let conn = Rc::new(conn);

            // Set the client connection on the agent for sending notifications.
            let client: Rc<dyn AcpClient> = Rc::new(ConnectionClient {
                conn: Rc::clone(&conn),
            });
            agent.set_client(client);

            // Spawn the engine worker with IO handles.
            let conn_for_worker = Rc::clone(&conn);
            tokio::task::spawn_local(async move {
                engine_worker(prompt_rx, conn_for_worker, io_input, io_output).await;
            });

            info!("ACP server ready, waiting for client connection");

            // Run the IO task (blocks until connection closes).
            match io_task.await {
                Ok(()) => {
                    info!("ACP connection closed cleanly");
                }
                Err(e) => {
                    error!(error = %e, "ACP connection error");
                    return Err(anyhow::anyhow!("ACP connection error: {e}"));
                }
            }

            Ok(())
        })
        .await;

    // Gracefully stop the engine.
    engine.stop();
    engine.shutdown().await;

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_hil_response_approvals() {
        assert!(is_hil_response("y"));
        assert!(is_hil_response("Y"));
        assert!(is_hil_response("yes"));
        assert!(is_hil_response("YES"));
        assert!(is_hil_response("Yes"));
    }

    #[test]
    fn is_hil_response_rejections() {
        assert!(is_hil_response("n"));
        assert!(is_hil_response("N"));
        assert!(is_hil_response("no"));
        assert!(is_hil_response("NO"));
        assert!(is_hil_response("No"));
    }

    #[test]
    fn is_hil_response_with_whitespace() {
        assert!(is_hil_response(" y "));
        assert!(is_hil_response("  no  "));
    }

    #[test]
    fn is_hil_response_non_hil() {
        assert!(!is_hil_response("hello"));
        assert!(!is_hil_response("yeah"));
        assert!(!is_hil_response("nope"));
        assert!(!is_hil_response("y req-123"));
        assert!(!is_hil_response(""));
    }
}
