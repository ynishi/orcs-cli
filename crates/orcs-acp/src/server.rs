//! ACP stdio server entry point.
//!
//! Starts the ACP server on stdin/stdout using `AgentSideConnection`
//! from the `agent-client-protocol` crate.

use std::rc::Rc;

use agent_client_protocol::{
    AgentSideConnection, Client, PromptResponse, SessionNotification, StopReason,
};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tracing::{error, info};

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

/// Processes prompt jobs by sending text to the ORCS engine and
/// streaming output back as ACP session updates.
///
/// This function runs in a loop, receiving jobs from the agent and
/// dispatching them. In P1, it uses a simple request-response model
/// (no streaming yet).
async fn engine_worker(mut rx: mpsc::Receiver<PromptJob>, conn: Rc<AgentSideConnection>) {
    while let Some(job) = rx.recv().await {
        let session_id = &job.session_id;

        // Send the agent's response as a message chunk.
        let update =
            convert::text_to_agent_message_chunk(&format!("[ORCS] Received: {}", job.text));

        let notification = SessionNotification::new(
            agent_client_protocol::SessionId::new(session_id.clone()),
            update,
        );

        if let Err(e) = conn.session_notification(notification).await {
            error!(session_id = %session_id, error = %e, "failed to send session update");
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
/// # Errors
///
/// Returns error if the server encounters a fatal I/O error.
pub async fn run_acp_server() -> anyhow::Result<()> {
    info!("Starting ORCS ACP server on stdio");

    let local = LocalSet::new();

    local
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

            // Spawn the engine worker.
            let conn_for_worker = Rc::clone(&conn);
            tokio::task::spawn_local(async move {
                engine_worker(prompt_rx, conn_for_worker).await;
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
        .await
}
