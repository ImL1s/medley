use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

use crate::{
    AcpMethod, acp_send,
    common::{AcpChannelFailure, AcpResult, acp_channel_failure_error},
    message::{AcpAgentMessage, AcpArgs, AcpClientMessage, AcpRequest, AcpSide},
};

type SpawnFn = Rc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)>;
/// Callback that creates a `tracing::Span` from `_meta` for distributed tracing.
type OnMetaFn = Rc<dyn Fn(&acp::Meta) -> tracing::Span>;

/// Gateway receiver - allows sending messages to it via a channel and it will
/// forward them to an underlying connection.
pub struct AcpGatewayReceiver<S: AcpSide, C> {
    rx: GatewayRx<S::OutMessage>,
    conn: C,
    tracing: bool,
    spawn_fn: SpawnFn,
    on_meta: Option<OnMetaFn>,
}

enum GatewayRx<T> {
    Unbounded(mpsc::UnboundedReceiver<T>),
    Bounded(mpsc::Receiver<T>),
}

enum GatewayTx<T> {
    Unbounded(mpsc::UnboundedSender<T>),
    Bounded(mpsc::Sender<T>),
}

impl<T> Clone for GatewayTx<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Unbounded(tx) => Self::Unbounded(tx.clone()),
            Self::Bounded(tx) => Self::Bounded(tx.clone()),
        }
    }
}

impl<S: AcpSide, C> AcpGatewayReceiver<S, C> {
    pub fn new(rx: mpsc::UnboundedReceiver<S::OutMessage>, conn: C) -> Self {
        Self::new_with_rx(GatewayRx::Unbounded(rx), conn)
    }

    pub fn new_bounded(rx: mpsc::Receiver<S::OutMessage>, conn: C) -> Self {
        Self::new_with_rx(GatewayRx::Bounded(rx), conn)
    }

    fn new_with_rx(rx: GatewayRx<S::OutMessage>, conn: C) -> Self {
        Self {
            rx,
            conn,
            tracing: false,
            spawn_fn: Rc::new(|fut| {
                tokio::task::spawn_local(fut);
            }),
            on_meta: None,
        }
    }

    pub fn with_tracing(mut self, tracing: bool) -> Self {
        self.tracing = tracing;
        self
    }

    /// Override the spawner used for dispatching incoming messages.
    ///
    /// By default, `spawn_local` is used (suitable for `LocalSet` runtimes).
    /// Pass a custom spawner to use a different execution strategy.
    pub fn with_spawn_fn(
        mut self,
        f: impl Fn(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    ) -> Self {
        self.spawn_fn = Rc::new(f);
        self
    }

    /// Hook that builds a `tracing::Span` from `_meta` to `.instrument()` dispatched messages.
    pub fn with_on_meta(mut self, f: impl Fn(&acp::Meta) -> tracing::Span + 'static) -> Self {
        self.on_meta = Some(Rc::new(f));
        self
    }
}

async fn recv_from_gateway_rx<T>(rx: &mut GatewayRx<T>) -> Option<T> {
    match rx {
        GatewayRx::Unbounded(rx) => rx.recv().await,
        GatewayRx::Bounded(rx) => rx.recv().await,
    }
}

/// The other side of the gateway. Allows to send messages to a channel so that
/// they will be forwarded automatically to a connection (as long as gateway
/// receiver side is running in the background).
pub struct AcpGatewaySender<S: AcpSide> {
    tx: GatewayTx<S::OutMessage>,
    tracing: bool,
}

impl<S: AcpSide> Clone for AcpGatewaySender<S> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            tracing: self.tracing,
        }
    }
}

impl<S: AcpSide> AcpGatewaySender<S> {
    pub fn new(tx: mpsc::UnboundedSender<S::OutMessage>) -> Self {
        Self {
            tx: GatewayTx::Unbounded(tx),
            tracing: false,
        }
    }

    pub fn new_bounded(tx: mpsc::Sender<S::OutMessage>) -> Self {
        Self {
            tx: GatewayTx::Bounded(tx),
            tracing: false,
        }
    }

    pub fn tx(&self) -> mpsc::UnboundedSender<S::OutMessage> {
        match &self.tx {
            GatewayTx::Unbounded(tx) => tx.clone(),
            GatewayTx::Bounded(_) => {
                panic!("AcpGatewaySender::tx is only available for unbounded senders")
            }
        }
    }

    pub fn with_tracing(mut self, tracing: bool) -> Self {
        self.tracing = tracing;
        self
    }
}

pub fn acp_gateway<S: AcpSide, C>(conn: C) -> (AcpGatewaySender<S>, AcpGatewayReceiver<S, C>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let sender = AcpGatewaySender::new(tx);
    let receiver = AcpGatewayReceiver::new(rx, conn);
    (sender, receiver)
}

pub type AcpAgentGatewayReceiver = AcpGatewayReceiver<acp::AgentSide, acp::AgentSideConnection>;
pub type AcpAgentGatewaySender = AcpGatewaySender<acp::AgentSide>;
pub type AcpClientGatewayReceiver = AcpGatewayReceiver<acp::ClientSide, acp::ClientSideConnection>;
pub type AcpClientGatewaySender = AcpGatewaySender<acp::ClientSide>;

fn before_request<T: AcpRequest>(args: &AcpArgs<T>, tracing: bool) -> Option<&'static str> {
    tracing.then(|| {
        let method = args.method_name();
        tracing::debug!(event = "outbound_request", method, request_present = true);
        method
    })
}

fn after_request<T>(
    response_tx: oneshot::Sender<AcpResult<T>>,
    response: AcpResult<T>,
    method: Option<&'static str>,
) -> bool {
    if let Some(method) = method {
        match &response {
            Ok(_) => {
                tracing::debug!(
                    event = "outbound_response",
                    method,
                    outcome = "success",
                    response_present = true
                );
            }
            Err(_) => {
                tracing::debug!(
                    event = "outbound_response",
                    method,
                    outcome = "error",
                    response_present = false
                );
            }
        }
    }
    response_tx.send(response).is_ok()
}

macro_rules! handle {
    ($args:expr, $tracing:expr, $conn:expr, $name:ident, $spawn:expr, $on_meta:expr $(,)?) => {{
        let span = ($on_meta)
            .as_ref()
            .zip(($args).request.meta.as_ref())
            .map(|(f, meta)| f(meta))
            .unwrap_or_else(tracing::Span::none);
        ($spawn)(Box::pin(
            async move {
                let method = before_request(&($args), $tracing);
                let response = ($conn).$name(($args).request).await;
                let _ = after_request(($args).response_tx, response, method);
            }
            .instrument(span),
        ));
    }};
    // Variant for types without `meta` field (ExtRequest, ExtNotification).
    // $on_meta is accepted (but unused) to disambiguate from the primary pattern.
    (no_meta, $args:expr, $tracing:expr, $conn:expr, $name:ident, $spawn:expr, $on_meta:expr $(,)?) => {
        ($spawn)(Box::pin(async move {
            let method = before_request(&($args), $tracing);
            let response = ($conn).$name(($args).request).await;
            let _ = after_request(($args).response_tx, response, method);
        }));
    };
}

impl<C: acp::Agent + 'static> AcpGatewayReceiver<acp::ClientSide, C> {
    pub async fn run(self) {
        let AcpGatewayReceiver {
            mut rx,
            conn,
            tracing,
            spawn_fn,
            on_meta,
        } = self;
        let conn = Rc::new(conn);
        let spawn = spawn_fn;
        while let Some(msg) = recv_from_gateway_rx(&mut rx).await {
            let conn = conn.clone();
            match msg {
                AcpAgentMessage::Initialize(args) => {
                    handle!(args, tracing, conn, initialize, spawn, on_meta);
                }
                AcpAgentMessage::Authenticate(args) => {
                    handle!(args, tracing, conn, authenticate, spawn, on_meta);
                }
                AcpAgentMessage::NewSession(args) => {
                    handle!(args, tracing, conn, new_session, spawn, on_meta);
                }
                AcpAgentMessage::LoadSession(args) => {
                    handle!(args, tracing, conn, load_session, spawn, on_meta);
                }
                AcpAgentMessage::SetSessionMode(args) => {
                    handle!(args, tracing, conn, set_session_mode, spawn, on_meta);
                }
                AcpAgentMessage::Prompt(args) => {
                    handle!(args, tracing, conn, prompt, spawn, on_meta);
                }
                AcpAgentMessage::Cancel(args) => {
                    handle!(args, tracing, conn, cancel, spawn, on_meta);
                }
                AcpAgentMessage::ExtMethod(args) => {
                    handle!(no_meta, args, tracing, conn, ext_method, spawn, on_meta);
                }
                AcpAgentMessage::ExtNotification(args) => {
                    handle!(
                        no_meta,
                        args,
                        tracing,
                        conn,
                        ext_notification,
                        spawn,
                        on_meta
                    );
                }
                AcpAgentMessage::SetSessionModel(args) => {
                    handle!(args, tracing, conn, set_session_model, spawn, on_meta);
                }
            }
        }
        if tracing {
            tracing::trace!("stopping gateway loop: receiver channel is closed");
        }
    }
}

impl<C: acp::Client + 'static> AcpGatewayReceiver<acp::AgentSide, C> {
    pub async fn run(self) {
        let AcpGatewayReceiver {
            mut rx,
            conn,
            tracing,
            spawn_fn,
            on_meta,
        } = self;
        let conn = Rc::new(conn);
        let spawn = spawn_fn;
        while let Some(msg) = recv_from_gateway_rx(&mut rx).await {
            let conn = conn.clone();
            match msg {
                AcpClientMessage::RequestPermission(args) => {
                    handle!(args, tracing, conn, request_permission, spawn, on_meta);
                }
                AcpClientMessage::ReadTextFile(args) => {
                    handle!(args, tracing, conn, read_text_file, spawn, on_meta);
                }
                AcpClientMessage::WriteTextFile(args) => {
                    handle!(args, tracing, conn, write_text_file, spawn, on_meta);
                }
                AcpClientMessage::SessionNotification(args) => {
                    handle!(args, tracing, conn, session_notification, spawn, on_meta);
                }
                AcpClientMessage::CreateTerminal(args) => {
                    handle!(args, tracing, conn, create_terminal, spawn, on_meta);
                }
                AcpClientMessage::TerminalOutput(args) => {
                    handle!(args, tracing, conn, terminal_output, spawn, on_meta);
                }
                AcpClientMessage::ReleaseTerminal(args) => {
                    handle!(args, tracing, conn, release_terminal, spawn, on_meta);
                }
                AcpClientMessage::WaitForTerminalExit(args) => {
                    handle!(args, tracing, conn, wait_for_terminal_exit, spawn, on_meta);
                }
                AcpClientMessage::KillTerminalCommand(args) => {
                    handle!(args, tracing, conn, kill_terminal, spawn, on_meta);
                }
                AcpClientMessage::ExtMethod(args) => {
                    handle!(no_meta, args, tracing, conn, ext_method, spawn, on_meta);
                }
                AcpClientMessage::ExtNotification(args) => {
                    handle!(
                        no_meta,
                        args,
                        tracing,
                        conn,
                        ext_notification,
                        spawn,
                        on_meta
                    );
                }
            }
        }
        if tracing {
            tracing::trace!("stopping gateway loop: receiver channel is closed");
        }
    }
}

impl<S: AcpSide> AcpGatewaySender<S> {
    fn try_enqueue_out_message(&self, msg: S::OutMessage) -> Result<(), &'static str> {
        match &self.tx {
            GatewayTx::Unbounded(tx) => tx.send(msg).map_err(|_| "receiver dropped"),
            GatewayTx::Bounded(tx) => tx.try_send(msg).map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => "queue full",
                mpsc::error::TrySendError::Closed(_) => "receiver dropped",
            }),
        }
    }

    /// Shared enqueue for the forward variants; `caller` attributes the
    /// dropped-receiver log to the right public method.
    fn enqueue<T>(
        &self,
        request: T,
        caller: &'static str,
    ) -> (bool, oneshot::Receiver<AcpResult<T::Response>>)
    where
        T: AcpRequest,
        S::OutMessage: From<AcpArgs<T>>,
    {
        let (response_tx, response_rx) = oneshot::channel();
        let method = request.method_name();
        let args = AcpArgs {
            request,
            response_tx,
        };
        let enqueue = self.try_enqueue_out_message(args.into());
        if let Err(reason) = enqueue {
            tracing::debug!(method, "{caller}: {reason}, notification discarded");
        }
        (enqueue.is_ok(), response_rx)
    }

    /// Enqueue a request and return a completion receiver for handler finish.
    pub fn forward_with_completion<T>(
        &self,
        request: T,
    ) -> oneshot::Receiver<AcpResult<T::Response>>
    where
        T: AcpRequest,
        S::OutMessage: From<AcpArgs<T>>,
    {
        self.enqueue(request, "forward_with_completion").1
    }

    /// Enqueue a request without waiting for the response. Returns whether
    /// the gateway channel accepted it (`false`: receiver gone, message
    /// discarded) so callers keeping delivery-dependent state can retry.
    pub fn forward_fire_and_forget<T>(&self, request: T) -> bool
    where
        T: AcpRequest,
        S::OutMessage: From<AcpArgs<T>>,
    {
        self.enqueue(request, "forward_fire_and_forget").0
    }

    /// Send a request and await the response. Returns a `Send` future.
    ///
    /// Equivalent to the `acp::Client` / `acp::Agent` trait methods but the
    /// returned future is `Send` because this is an inherent async fn — not
    /// wrapped by `#[async_trait(?Send)]`.
    pub async fn send<T>(&self, request: T) -> AcpResult<T::Response>
    where
        T: AcpRequest,
        S::OutMessage: From<AcpArgs<T>>,
    {
        self.forward(request).await
    }

    async fn forward<T>(&self, request: T) -> AcpResult<T::Response>
    where
        T: AcpRequest,
        S::OutMessage: From<AcpArgs<T>>,
    {
        if self.tracing {
            tracing::debug!(
                event = "inbound_request",
                method = request.method_name(),
                request_present = true
            );
        }
        match &self.tx {
            GatewayTx::Unbounded(tx) => acp_send(request, tx).await,
            GatewayTx::Bounded(tx) => acp_send_bounded(request, tx).await,
        }
    }
}

async fn acp_send_bounded<R, T>(request: T, tx: &mpsc::Sender<R>) -> AcpResult<T::Response>
where
    T: AcpRequest,
    R: From<AcpArgs<T>> + fmt::Debug,
{
    let (response_tx, response_rx) = oneshot::channel();
    let method = request.method_name();
    let args = AcpArgs {
        request,
        response_tx,
    };

    tx.try_send(args.into()).map_err(|err| match err {
        mpsc::error::TrySendError::Full(_) => acp_channel_failure_error(
            format!("unable to send '{method}' request, channel full"),
            AcpChannelFailure::SendFailed,
        ),
        mpsc::error::TrySendError::Closed(_) => acp_channel_failure_error(
            format!("unable to send '{method}' request, channel closed"),
            AcpChannelFailure::SendFailed,
        ),
    })?;

    response_rx.await.map_err(|_| {
        acp_channel_failure_error(
            format!("unable to receive '{method}' response, channel closed"),
            AcpChannelFailure::RecvFailed,
        )
    })?
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpGatewaySender<acp::AgentSide> {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> AcpResult<acp::RequestPermissionResponse> {
        self.forward(args).await
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> AcpResult<acp::WriteTextFileResponse> {
        self.forward(args).await
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> AcpResult<acp::ReadTextFileResponse> {
        self.forward(args).await
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> AcpResult<acp::CreateTerminalResponse> {
        self.forward(args).await
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> AcpResult<acp::TerminalOutputResponse> {
        self.forward(args).await
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> AcpResult<acp::ReleaseTerminalResponse> {
        self.forward(args).await
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> AcpResult<acp::WaitForTerminalExitResponse> {
        self.forward(args).await
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> AcpResult<acp::KillTerminalResponse> {
        self.forward(args).await
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> AcpResult<()> {
        // Fire-and-forget: session notifications carry no meaningful response (the
        // ACK is `()`), so we must not block the caller waiting for the client to
        // acknowledge.  When the agent→relay→client path is degraded (e.g. a Slack
        // session whose ephemeral WebSocket died mid-turn), the relay write can
        // stall for minutes (TCP retransmit timeout).  Blocking here freezes the
        // terminal streaming loop — its timeout check never fires, the session
        // actor can't process new prompts, and the entire session hangs.
        self.forward_fire_and_forget(args);
        Ok(())
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> AcpResult<acp::ExtResponse> {
        self.forward(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> AcpResult<()> {
        // Fire-and-forget for the same reason as `session_notification` above:
        // the ACK is `()` and blocking risks hanging the caller when the
        // relay→client path is degraded.  Many call sites already bypass this
        // trait method and call `forward_fire_and_forget` directly.
        self.forward_fire_and_forget(args);
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for AcpGatewaySender<acp::ClientSide> {
    async fn initialize(&self, args: acp::InitializeRequest) -> AcpResult<acp::InitializeResponse> {
        self.forward(args).await
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> AcpResult<acp::AuthenticateResponse> {
        self.forward(args).await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> AcpResult<acp::NewSessionResponse> {
        self.forward(args).await
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> AcpResult<acp::LoadSessionResponse> {
        self.forward(args).await
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> AcpResult<acp::SetSessionModeResponse> {
        self.forward(args).await
    }

    async fn prompt(&self, args: acp::PromptRequest) -> AcpResult<acp::PromptResponse> {
        self.forward(args).await
    }

    async fn cancel(&self, args: acp::CancelNotification) -> AcpResult<()> {
        self.forward(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> AcpResult<acp::ExtResponse> {
        self.forward(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> AcpResult<()> {
        self.forward(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use agent_client_protocol as acp;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::registry::LookupSpan;

    struct OrderTrackingClient {
        log: Rc<RefCell<Vec<String>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for OrderTrackingClient {
        async fn request_permission(
            &self,
            _: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            unimplemented!()
        }
        async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
            if let acp::SessionUpdate::AgentMessageChunk(chunk) = &args.update
                && let acp::ContentBlock::Text(text) = &chunk.content
            {
                self.log.borrow_mut().push(text.text.clone());
            }
            Ok(())
        }
    }

    fn text_notification(marker: &str) -> acp::SessionNotification {
        acp::SessionNotification::new(
            acp::SessionId::new("s"),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(marker),
            ))),
        )
    }

    #[derive(Default)]
    struct EventCollector {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for EventCollector {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = EventVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.fields.join(" "));
        }
    }

    #[derive(Default)]
    struct EventVisitor {
        fields: Vec<String>,
    }

    impl Visit for EventVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields.push(format!("{}={value:?}", field.name()));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields.push(format!("{}={value}", field.name()));
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }

    fn assert_secret_absent(rendered: &str, secret: &str) {
        assert!(!rendered.contains(secret), "leaked full secret: {rendered}");
        for window in secret.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ASCII sentinel window");
            assert!(
                !rendered.contains(window),
                "leaked secret window {window:?}: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn gateway_observability_omits_mcp_header_env_response_and_error_secrets() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let header_secret = "GB002-header-secret-0123456789abcdef";
        let env_secret = "GB002-env-secret-fedcba9876543210";
        let response_secret = "Z9Y8X7W6V5U4T3S2R1Q0P9O8N7M6";
        let error_secret = "M6N7O8P9Q0R1S2T3U4V5W6X7Y8Z9";

        let events = Arc::new(Mutex::new(Vec::new()));
        let _guard = tracing_subscriber::registry()
            .with(EventCollector {
                events: events.clone(),
            })
            .set_default();

        let mcp_servers = vec![
            acp::McpServer::Http(
                acp::McpServerHttp::new("private-http", "https://example.invalid")
                    .headers(vec![acp::HttpHeader::new("Authorization", header_secret)]),
            ),
            acp::McpServer::Stdio(
                acp::McpServerStdio::new("private-stdio", "/bin/false")
                    .env(vec![acp::EnvVariable::new("PRIVATE_TOKEN", env_secret)]),
            ),
        ];
        let new_session = acp::NewSessionRequest::new("/tmp").mcp_servers(mcp_servers.clone());
        let load_session =
            acp::LoadSessionRequest::new("session-id", "/tmp").mcp_servers(mcp_servers);

        let (new_tx, _new_rx) = oneshot::channel();
        let new_args = AcpArgs {
            request: new_session.clone(),
            response_tx: new_tx,
        };
        assert_eq!(before_request(&new_args, true), Some("session/new"));

        let (load_tx, _load_rx) = oneshot::channel();
        let load_args = AcpArgs {
            request: load_session.clone(),
            response_tx: load_tx,
        };
        assert_eq!(before_request(&load_args, true), Some("session/load"));

        let (success_tx, _success_rx) = oneshot::channel();
        assert!(after_request(
            success_tx,
            Ok(response_secret.to_string()),
            Some("session/new")
        ));
        let (error_tx, _error_rx) = oneshot::channel::<AcpResult<String>>();
        assert!(after_request(
            error_tx,
            Err(crate::common::acp_internal_error(error_secret)),
            Some("session/load")
        ));

        let (gateway_tx, gateway_rx) = mpsc::unbounded_channel::<AcpAgentMessage>();
        drop(gateway_rx);
        let sender = AcpGatewaySender::<acp::ClientSide>::new(gateway_tx).with_tracing(true);
        assert!(sender.send(new_session).await.is_err());
        assert!(sender.send(load_session).await.is_err());

        let rendered = events.lock().unwrap().join("\n");
        for secret in [header_secret, env_secret, response_secret, error_secret] {
            assert_secret_absent(&rendered, secret);
        }
        assert!(rendered.contains("event=outbound_request"), "{rendered}");
        assert!(rendered.contains("event=outbound_response"), "{rendered}");
        assert!(rendered.contains("event=inbound_request"), "{rendered}");
        assert!(rendered.contains("method=session/new"), "{rendered}");
        assert!(rendered.contains("method=session/load"), "{rendered}");
        assert!(rendered.contains("outcome=success"), "{rendered}");
        assert!(rendered.contains("outcome=error"), "{rendered}");
    }

    /// Regression: draining completion receivers preserves notification ordering.
    #[tokio::test]
    async fn completion_drain_preserves_notification_ordering() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let log = Rc::new(RefCell::new(Vec::<String>::new()));
                let (sender, receiver) =
                    acp_gateway::<acp::AgentSide, _>(OrderTrackingClient { log: log.clone() });
                tokio::task::spawn_local(receiver.run());

                const N: usize = 100;
                let completions: Vec<_> = (0..N)
                    .map(|i| sender.forward_with_completion(text_notification(&format!("{i}"))))
                    .collect();
                for rx in completions {
                    let _ = rx.await;
                }

                log.borrow_mut().push("RESPONSE".into());

                let log = log.borrow();
                assert_eq!(log.len(), N + 1);
                assert_eq!(log[N], "RESPONSE");
                for i in 0..N {
                    assert_eq!(log[i], format!("{i}"));
                }
            })
            .await;
    }

    /// Regression: two-phase cutover keeps replay-before-response and avoids
    /// dropping live updates during drain.
    #[tokio::test]
    async fn two_phase_cutover_no_missing_updates() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let log = Rc::new(RefCell::new(Vec::<String>::new()));
                let (sender, receiver) =
                    acp_gateway::<acp::AgentSide, _>(OrderTrackingClient { log: log.clone() });
                tokio::task::spawn_local(receiver.run());

                const DELTA: usize = 50;
                const LIVE: usize = 20;

                // Phase 1: sync enqueue of replay notifications.
                let completions: Vec<_> = (0..DELTA)
                    .map(|i| {
                        sender.forward_with_completion(text_notification(&format!("delta-{i}")))
                    })
                    .collect();

                // Gate-open point; then concurrent producer emits live updates.
                let live_sender = sender.clone();
                let producer = tokio::task::spawn_local(async move {
                    for i in 0..LIVE {
                        live_sender
                            .forward_fire_and_forget(text_notification(&format!("live-{i}")));
                        // Encourage interleaving with drain.
                        tokio::task::yield_now().await;
                    }
                });

                // Drain replay completions while producer runs.
                for rx in completions {
                    let _ = rx.await;
                }

                // Mark response boundary.
                log.borrow_mut().push("RESPONSE".into());

                // Let producer and gateway finish remaining live updates.
                let _ = producer.await;
                for _ in 0..LIVE + 5 {
                    tokio::task::yield_now().await;
                }

                let log = log.borrow();
                let response_idx = log
                    .iter()
                    .position(|s| s == "RESPONSE")
                    .expect("RESPONSE marker must be in the log");

                // (1) Delta notifications are all present and before RESPONSE.
                for i in 0..DELTA {
                    let tag = format!("delta-{i}");
                    let pos = log
                        .iter()
                        .position(|s| s == &tag)
                        .unwrap_or_else(|| panic!("missing delta notification: {tag}"));
                    assert!(
                        pos < response_idx,
                        "{tag} at index {pos} must precede RESPONSE at index {response_idx}"
                    );
                }

                // (2) Delta notifications preserve enqueue order.
                let delta_positions: Vec<usize> = (0..DELTA)
                    .map(|i| log.iter().position(|s| s == &format!("delta-{i}")).unwrap())
                    .collect();
                for w in delta_positions.windows(2) {
                    assert!(
                        w[0] < w[1],
                        "delta ordering violated: delta at index {} came after delta at index {}",
                        w[0],
                        w[1]
                    );
                }

                // (3) No live updates are lost.
                for i in 0..LIVE {
                    let tag = format!("live-{i}");
                    assert!(
                        log.iter().any(|s| s == &tag),
                        "live update lost: {tag} not found in log"
                    );
                }

                // (4) Live updates do not precede replay delta.
                let last_delta = *delta_positions.last().unwrap();
                for i in 0..LIVE {
                    let tag = format!("live-{i}");
                    let pos = log.iter().position(|s| s == &tag).unwrap();
                    assert!(
                        pos > last_delta,
                        "{tag} at index {pos} must come after last delta at index {last_delta}"
                    );
                }
            })
            .await;
    }
}
