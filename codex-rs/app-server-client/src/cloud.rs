//! Channel-backed cloud client. Requests and retained replies stay on one owner;
//! events are drained independently so a foreground request cannot block its response.

use crate::AppServerEvent;
use crate::RequestResult;
use crate::TypedRequestError;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStartParams;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio::time::timeout_at;

/// A local project's GitHub origins to provision in the default cloud environment.
/// Callers must supply credential-free origin URLs; this client does not inspect local Git state.
#[derive(Clone)]
pub struct ProjectEnvironment {
    pub name: String,
    pub origin_urls: Vec<String>,
    /// Working directory inside the managed environment, when a project selects one.
    pub cwd: Option<String>,
}

/// An explicit saved environment or the default environment with a project checkout.
#[derive(Clone, Debug)]
pub enum EnvironmentSelection {
    Saved(String),
    Project(ProjectEnvironment),
}

impl std::fmt::Debug for ProjectEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProjectEnvironment([redacted])")
    }
}

/// A saved environment available to the current Cloud account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedEnvironment {
    pub id: String,
    pub name: String,
    pub workspace_shared: bool,
}

/// Rejection from the original network policy during Cloud recovery.
pub const CLOUD_RECOVERY_NETWORK_BLOCKED: &str =
    "The current network policy blocks cloud reconnection.";

/// Operations the cloud worker must handle without forwarding to another backend.
/// Unsupported operations must return an explicit error; they must not claim success.
pub enum CloudOperation {
    Request(Box<ClientRequest>),
    /// Opens a provisional attachment and serves its initial history on a scoped channel.
    PrepareThreadResume {
        request_id: RequestId,
        params: Box<codex_app_server_protocol::ThreadResumeParams>,
        history: mpsc::Receiver<CloudCommand>,
    },
    /// Sent on the scoped channel after the caller has prepared the replacement view.
    CommitPreparedResume,
    /// Environment selection stays on the private transport, outside the app-server wire.
    StartThreadWithEnvironment {
        request_id: RequestId,
        params: Box<codex_app_server_protocol::ThreadStartParams>,
        environment: EnvironmentSelection,
    },
    SubmitUserMessage(Box<TurnStartParams>),
    /// Apply the result of a name write that already succeeded outside the command queue.
    ThreadNameSaved(ThreadSetNameParams),
    ResumeDisconnectedThread {
        thread_id: String,
        require_history: bool,
        deadline: Instant,
        release: oneshot::Receiver<oneshot::Sender<Result<(), &'static str>>>,
    },
    Notify(ClientNotification),
    Resolve {
        request_id: RequestId,
        result: JsonRpcResult,
    },
    Reject {
        request_id: RequestId,
        error: JSONRPCErrorError,
    },
    Shutdown,
}

/// One server-decided admission, not completion. A duplicate carries identity
/// only; read history or observe the reply stream before inferring its status.
#[derive(serde::Serialize, serde::Deserialize)]
pub enum CloudUserMessageReceipt {
    Started(Turn),
    Steered(Turn),
    Duplicate { turn_id: Option<String> },
}

/// An operation and its single response. Dropping a caller does not undo admission;
/// cloud workers must not retry mutations when the receiver has gone away.
pub struct CloudCommand {
    pub operation: CloudOperation,
    pub response: oneshot::Sender<io::Result<RequestResult>>,
}

/// Allows the worker to install a held stream after the caller reads its history.
/// Dropping the token cancels that replacement.
pub struct CloudRecoveryRelease {
    release: oneshot::Sender<oneshot::Sender<Result<(), &'static str>>>,
    deadline: Instant,
}

impl CloudRecoveryRelease {
    pub async fn install(self) -> Result<(), &'static str> {
        let (ack, mut installed) = oneshot::channel();
        self.release
            .send(ack)
            .map_err(|_| "Cloud reconnection expired.")?;
        let response = match timeout_at(self.deadline, &mut installed).await {
            Ok(result) => result.ok(),
            Err(_) => {
                installed.close();
                installed.try_recv().ok()
            }
        };
        response.unwrap_or(Err("Cloud reconnection expired."))
    }
}

#[derive(Clone, Default)]
pub(super) enum IdleReconnect {
    #[default]
    Inactive,
    Offered(Arc<str>),
    Opening(Arc<str>),
    Connected(Arc<str>),
}

#[derive(Clone)]
pub struct CloudRequestHandle {
    pub(super) reconnect: tokio::sync::watch::Sender<IdleReconnect>,
    commands: mpsc::Sender<CloudCommand>,
}

/// Tracks only losses queued for delivery. The worker remains the sole owner
/// of the active attachment. A successful replacement supersedes earlier losses
/// for that conversation, even if the replacement itself later ends.
#[derive(Clone, Default)]
pub(super) struct CloudStreamLosses(Arc<Mutex<HashMap<String, PendingLosses>>>);

#[derive(Default)]
struct PendingLosses {
    remaining: usize,
    superseded: usize,
}

impl CloudStreamLosses {
    fn pending(&self) -> MutexGuard<'_, HashMap<String, PendingLosses>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn record(&self, thread_id: &str) {
        self.pending()
            .entry(thread_id.to_owned())
            .or_default()
            .remaining += 1;
    }

    pub(super) fn supersede(&self, thread_id: &str) {
        if let Some(losses) = self.pending().get_mut(thread_id) {
            losses.superseded = losses.remaining;
        }
    }

    fn should_deliver(&self, thread_id: &str) -> bool {
        let mut pending = self.pending();
        let Some(losses) = pending.get_mut(thread_id) else {
            return true;
        };
        // A single worker sends losses in the same order as it records them.
        let deliver = losses.superseded == 0;
        losses.superseded = losses.superseded.saturating_sub(1);
        losses.remaining -= 1;
        if losses.remaining == 0 {
            pending.remove(thread_id);
        }
        deliver
    }
}

pub struct CloudClient {
    pub(super) stream_losses: CloudStreamLosses,
    handle: CloudRequestHandle,
    events: mpsc::UnboundedReceiver<AppServerEvent>,
}

impl CloudClient {
    /// Creates a cloud transport. The owner must drain commands and stop when
    /// its receiver closes. Event delivery is independent of request completion,
    /// matching conventional transports; cloud workers must bound remote payloads.
    pub fn channel(
        capacity: usize,
    ) -> (
        Self,
        mpsc::Receiver<CloudCommand>,
        mpsc::UnboundedSender<AppServerEvent>,
    ) {
        let (commands, receiver) = mpsc::channel(capacity.max(1));
        let (events, event_receiver) = mpsc::unbounded_channel();
        (
            Self {
                stream_losses: CloudStreamLosses::default(),
                handle: CloudRequestHandle {
                    commands,
                    reconnect: Default::default(),
                },
                events: event_receiver,
            },
            receiver,
            events,
        )
    }

    pub(crate) fn request_handle(&self) -> CloudRequestHandle {
        self.handle.clone()
    }

    pub(crate) async fn next_event(&mut self) -> Option<AppServerEvent> {
        while let Some(event) = self.events.recv().await {
            if let AppServerEvent::ThreadStreamDisconnected(error) = &event
                && !self.stream_losses.should_deliver(&error.thread_id)
            {
                continue;
            }
            if let AppServerEvent::ThreadStreamReconnected(id) = &event
                && !self.handle.reconnect.send_if_modified(|state| {
                    if matches!(state, IdleReconnect::Connected(root) if root.as_ref() == id) {
                        *state = IdleReconnect::Inactive;
                        return true;
                    }
                    false
                })
            {
                continue;
            }
            return Some(event);
        }
        None
    }

    pub(crate) async fn request(&self, request: ClientRequest) -> io::Result<RequestResult> {
        self.handle.request(request).await
    }

    pub(crate) async fn notify(&self, notification: ClientNotification) -> io::Result<()> {
        self.handle.unit(CloudOperation::Notify(notification)).await
    }

    pub(crate) async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> io::Result<()> {
        self.handle.resolve_server_request(request_id, result).await
    }

    pub(crate) async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> io::Result<()> {
        self.handle.reject_server_request(request_id, error).await
    }

    pub(crate) async fn shutdown(self) -> io::Result<()> {
        tokio::time::timeout(
            crate::SHUTDOWN_TIMEOUT,
            self.handle.unit(CloudOperation::Shutdown),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "cloud backend shutdown timed out"))?
    }
}

impl CloudRequestHandle {
    /// Accept a quiet Attach only for the conversation displayed by the caller.
    pub fn reconnect_idle_thread(&self, thread_id: &str) -> bool {
        self.reconnect.send_if_modified(|state| {
            if let IdleReconnect::Offered(root) = state
                && root.as_ref() == thread_id
            {
                *state = IdleReconnect::Opening(Arc::clone(root));
                return true;
            }
            false
        })
    }

    pub async fn resume_disconnected_thread(
        &self,
        thread_id: String,
        require_history: bool,
        deadline: Instant,
    ) -> Result<(Option<ThreadResumeResponse>, CloudRecoveryRelease), TypedRequestError> {
        let (release, held) = oneshot::channel();
        let response = timeout_at(
            deadline,
            self.send(CloudOperation::ResumeDisconnectedThread {
                thread_id,
                require_history,
                deadline,
                release: held,
            }),
        )
        .await
        .map_err(io::Error::from)
        .and_then(std::convert::identity);
        let settings: Option<ThreadResumeResponse> =
            crate::decode_typed_response("thread/resume", response)?;
        Ok((settings, CloudRecoveryRelease { release, deadline }))
    }

    pub(crate) async fn start_thread_with_environment(
        &self,
        request_id: RequestId,
        params: codex_app_server_protocol::ThreadStartParams,
        environment: EnvironmentSelection,
    ) -> io::Result<RequestResult> {
        self.send(CloudOperation::StartThreadWithEnvironment {
            request_id,
            params: Box::new(params),
            environment,
        })
        .await
    }

    /// Submit once with a stable message ID and saved thread settings. The server
    /// starts or steers; an error can mean admission is unknown. Never auto-resend.
    pub async fn submit_user_message(
        &self,
        params: TurnStartParams,
    ) -> Result<CloudUserMessageReceipt, TypedRequestError> {
        let response = self
            .send(CloudOperation::SubmitUserMessage(Box::new(params)))
            .await;
        crate::decode_typed_response("cloud/submitUserMessage", response)
    }

    /// Returns the activation snapshot and a handle restricted to this resume's history.
    /// Dropping the scoped handle without committing discards the new attachment.
    pub async fn prepare_thread_resume(
        &self,
        request_id: RequestId,
        params: codex_app_server_protocol::ThreadResumeParams,
    ) -> Result<(codex_app_server_protocol::ThreadResumeResponse, Self), TypedRequestError> {
        let (commands, history) = mpsc::channel(/*capacity*/ 1);
        let response = self
            .send(CloudOperation::PrepareThreadResume {
                request_id,
                params: Box::new(params),
                history,
            })
            .await;
        let snapshot = crate::decode_typed_response("thread/resume", response)?;
        Ok((
            snapshot,
            Self {
                commands,
                reconnect: self.reconnect.clone(),
            },
        ))
    }

    /// Installs the prepared attachment on the worker before its next ordinary command.
    pub async fn commit_prepared_resume(&self) -> Result<(), TypedRequestError> {
        let response = self.send(CloudOperation::CommitPreparedResume).await;
        crate::decode_typed_response("thread/resume", response)
    }

    async fn send(&self, operation: CloudOperation) -> io::Result<RequestResult> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(CloudCommand {
                operation,
                response,
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "cloud backend is closed"))?;
        result.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "cloud backend dropped its response",
            )
        })?
    }

    async fn unit(&self, operation: CloudOperation) -> io::Result<()> {
        self.send(operation)
            .await?
            .map(|_| ())
            .map_err(|error| io::Error::other(error.message))
    }

    pub(crate) async fn request(&self, request: ClientRequest) -> io::Result<RequestResult> {
        self.send(CloudOperation::Request(Box::new(request))).await
    }

    pub(crate) async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> io::Result<()> {
        self.unit(CloudOperation::Resolve { request_id, result })
            .await
    }

    pub(crate) async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> io::Result<()> {
        self.unit(CloudOperation::Reject { request_id, error })
            .await
    }
}

#[cfg(test)]
#[path = "cloud_tests.rs"]
mod tests;
