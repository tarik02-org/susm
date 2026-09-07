#[cfg(not(windows))]
compile_error!("the Tonic named-pipe spike only runs on Windows");

use std::{
    error::Error,
    io,
    os::windows::io::AsRawHandle,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
    },
    sync::{mpsc, watch},
    time::{sleep, timeout},
};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{
    Request, Response, Status,
    transport::{Endpoint, Server, server::Connected},
};
use tower::service_fn;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, HLOCAL, LocalFree,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES,
            TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT},
        System::{
            Pipes::{
                GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
                ImpersonateNamedPipeClient,
            },
            Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken},
        },
    },
    core::{PCWSTR, PWSTR},
};

mod proto {
    tonic::include_proto!("susm.spike.v1");
}

use proto::{
    PingRequest, PingResponse, WatchRequest, WatchResponse,
    spike_service_client::SpikeServiceClient,
    spike_service_server::{SpikeService, SpikeServiceServer},
};

type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct UserSid(Arc<str>);

impl UserSid {
    fn from_windows_token(value: String) -> Self {
        Self(value.into())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
struct CallerIdentity {
    sid: UserSid,
    process_id: u32,
}

struct AuthenticatedPipe {
    pipe: NamedPipeServer,
    caller: CallerIdentity,
}

impl Connected for AuthenticatedPipe {
    type ConnectInfo = CallerIdentity;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.caller.clone()
    }
}

impl AsyncRead for AuthenticatedPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_read(context, buffer)
    }
}

impl AsyncWrite for AuthenticatedPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.pipe).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_shutdown(context)
    }
}

struct TokenHandle(HANDLE);

impl TokenHandle {
    fn get(&self) -> HANDLE {
        self.0
    }
}

impl Drop for TokenHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct PipeImpersonation {
    active: bool,
}

impl PipeImpersonation {
    fn begin(pipe: HANDLE) -> windows::core::Result<Self> {
        unsafe {
            ImpersonateNamedPipeClient(pipe)?;
        }

        Ok(Self { active: true })
    }

    fn finish(mut self) -> windows::core::Result<()> {
        unsafe {
            RevertToSelf()?;
        }
        self.active = false;

        Ok(())
    }
}

impl Drop for PipeImpersonation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }

        if unsafe { RevertToSelf() }.is_err() {
            std::process::abort();
        }
    }
}

#[derive(Clone)]
struct SpikeRpc {
    dropped_watches: mpsc::UnboundedSender<u64>,
}

struct DropNotifyingStream {
    inner: ReceiverStream<Result<WatchResponse, Status>>,
    watch_id: u64,
    dropped_watches: mpsc::UnboundedSender<u64>,
}

impl Stream for DropNotifyingStream {
    type Item = Result<WatchResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context)
    }
}

impl Drop for DropNotifyingStream {
    fn drop(&mut self) {
        let _ = self.dropped_watches.send(self.watch_id);
    }
}

#[tonic::async_trait]
impl SpikeService for SpikeRpc {
    type WatchStream = DropNotifyingStream;

    async fn ping(&self, request: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        let caller = request
            .extensions()
            .get::<CallerIdentity>()
            .ok_or_else(|| Status::unauthenticated("named-pipe caller identity is missing"))?
            .clone();
        let request = request.into_inner();

        Ok(Response::new(PingResponse {
            payload: request.payload,
            caller_sid: caller.sid.as_str().to_owned(),
            caller_process_id: caller.process_id,
        }))
    }

    async fn watch(
        &self,
        request: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        request
            .extensions()
            .get::<CallerIdentity>()
            .ok_or_else(|| Status::unauthenticated("named-pipe caller identity is missing"))?;
        let request = request.into_inner();

        if request.watch_id == 0 {
            return Err(Status::invalid_argument("watch_id must be non-zero"));
        }
        if !(1..=1_000).contains(&request.interval_milliseconds) {
            return Err(Status::invalid_argument(
                "interval_milliseconds must be between 1 and 1000",
            ));
        }

        let (events, receiver) = mpsc::channel(8);
        tokio::spawn(async move {
            let mut sequence = 1;
            let interval = Duration::from_millis(u64::from(request.interval_milliseconds));

            loop {
                if events
                    .send(Ok(WatchResponse {
                        watch_id: request.watch_id,
                        sequence,
                    }))
                    .await
                    .is_err()
                {
                    break;
                }

                sequence += 1;
                sleep(interval).await;
            }
        });

        Ok(Response::new(DropNotifyingStream {
            inner: ReceiverStream::new(receiver),
            watch_id: request.watch_id,
            dropped_watches: self.dropped_watches.clone(),
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let pipe_name = Arc::<str>::from(format!(r"\\.\pipe\susm-tonic-spike-{}", std::process::id()));
    let expected_sid = current_process_sid()?;
    let (dropped_watches_tx, mut dropped_watches_rx) = mpsc::unbounded_channel();
    let rpc = SpikeRpc {
        dropped_watches: dropped_watches_tx,
    };
    let (incoming_tx, incoming_rx) = mpsc::channel(4);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let listener = tokio::spawn(run_pipe_listener(
        pipe_name.clone(),
        expected_sid.clone(),
        incoming_tx,
        shutdown_rx.clone(),
    ));
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(SpikeServiceServer::new(rpc))
            .serve_with_incoming_shutdown(
                ReceiverStream::new(incoming_rx),
                wait_for_shutdown(shutdown_rx),
            )
            .await
    });

    let connector_pipe = pipe_name.clone();
    let channel = Endpoint::from_static("http://susm.local")
        .connect_with_connector(service_fn(move |_| {
            let pipe_name = connector_pipe.clone();
            async move { open_pipe_client(pipe_name).await.map(TokioIo::new) }
        }))
        .await?;
    let mut client = SpikeServiceClient::new(channel);

    let ping = client
        .ping(PingRequest {
            payload: "named-pipe unary".to_owned(),
        })
        .await?
        .into_inner();
    assert_eq!(ping.payload, "named-pipe unary");
    assert_eq!(ping.caller_sid, expected_sid.as_str());
    assert_eq!(ping.caller_process_id, std::process::id());
    println!(
        "ok unary: authenticated caller {} with process ID {}",
        ping.caller_sid, ping.caller_process_id
    );

    let first_watch_id = 1;
    let mut first_watch = client
        .watch(WatchRequest {
            watch_id: first_watch_id,
            interval_milliseconds: 10,
        })
        .await?
        .into_inner();
    for expected_sequence in 1..=3 {
        let event = first_watch
            .message()
            .await?
            .ok_or("watch stream ended before three events arrived")?;
        assert_eq!(event.watch_id, first_watch_id);
        assert_eq!(event.sequence, expected_sequence);
    }
    println!("ok streaming: received three ordered events");
    drop(first_watch);
    wait_for_dropped_watch(first_watch_id, &mut dropped_watches_rx).await?;

    let cancelled_watch_id = 2;
    let mut cancelled_watch = client
        .watch(WatchRequest {
            watch_id: cancelled_watch_id,
            interval_milliseconds: 10,
        })
        .await?
        .into_inner();
    cancelled_watch
        .message()
        .await?
        .ok_or("cancellation watch ended before its first event")?;
    drop(cancelled_watch);
    wait_for_dropped_watch(cancelled_watch_id, &mut dropped_watches_rx).await?;
    println!("ok cancellation: dropping the client stream dropped the server stream");

    drop(client);
    shutdown_tx.send(true)?;

    listener.await??;
    server.await??;

    println!("all Tonic named-pipe spike checks passed");

    Ok(())
}

async fn run_pipe_listener(
    pipe_name: Arc<str>,
    owning_sid: UserSid,
    incoming: mpsc::Sender<Result<AuthenticatedPipe, io::Error>>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut first_instance = true;

    loop {
        if *shutdown.borrow() {
            return Ok(());
        }

        let pipe = create_pipe_server(pipe_name.as_ref(), first_instance, &owning_sid)?;
        first_instance = false;

        tokio::select! {
            result = pipe.connect() => result?,
            result = shutdown.changed() => {
                result.map_err(|_| io::Error::other("shutdown sender dropped"))?;
                return Ok(());
            }
        }

        let caller = authenticate_pipe_client(&pipe).map_err(io::Error::other)?;
        if incoming
            .send(Ok(AuthenticatedPipe { pipe, caller }))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
}

fn create_pipe_server(
    pipe_name: &str,
    first_instance: bool,
    owning_sid: &UserSid,
) -> io::Result<NamedPipeServer> {
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", owning_sid.as_str());
    let sddl = sddl.encode_utf16().chain([0]).collect::<Vec<_>>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(io::Error::other)?;
    }

    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let result = unsafe {
        ServerOptions::new()
            .pipe_mode(PipeMode::Byte)
            .first_pipe_instance(first_instance)
            .reject_remote_clients(true)
            .max_instances(16)
            .create_with_security_attributes_raw(
                pipe_name,
                std::ptr::from_mut(&mut attributes).cast(),
            )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }

    result
}

async fn open_pipe_client(pipe_name: Arc<str>) -> io::Result<NamedPipeClient> {
    loop {
        let client = ClientOptions::new()
            .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
            .open(pipe_name.as_ref());

        match client {
            Ok(client) => {
                let mut server_process_id = 0;
                unsafe {
                    GetNamedPipeServerProcessId(
                        HANDLE(client.as_raw_handle()),
                        &mut server_process_id,
                    )
                    .map_err(io::Error::other)?;
                }
                if server_process_id != std::process::id() {
                    return Err(io::Error::other(format!(
                        "unexpected named-pipe server process ID {server_process_id}"
                    )));
                }

                return Ok(client);
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND.0 as i32)
                    || error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) =>
            {
                sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn authenticate_pipe_client(pipe: &NamedPipeServer) -> windows::core::Result<CallerIdentity> {
    let handle = HANDLE(pipe.as_raw_handle());
    let mut process_id = 0;
    unsafe {
        GetNamedPipeClientProcessId(handle, &mut process_id)?;
    }
    let impersonation = PipeImpersonation::begin(handle)?;
    let sid = current_thread_sid();
    impersonation.finish()?;

    Ok(CallerIdentity {
        sid: sid?,
        process_id,
    })
}

fn current_process_sid() -> windows::core::Result<UserSid> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
    }

    token_sid(&TokenHandle(token))
}

fn current_thread_sid() -> windows::core::Result<UserSid> {
    let mut token = HANDLE::default();
    unsafe {
        OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token)?;
    }

    token_sid(&TokenHandle(token))
}

fn token_sid(token: &TokenHandle) -> windows::core::Result<UserSid> {
    let mut required_bytes = 0;
    unsafe {
        let _ = GetTokenInformation(token.get(), TokenUser, None, 0, &mut required_bytes);
    }

    if required_bytes == 0 {
        return Err(windows::core::Error::from_thread());
    }

    let word_size = size_of::<usize>();
    let word_count = (required_bytes as usize).div_ceil(word_size);
    let mut storage = vec![0usize; word_count];
    unsafe {
        GetTokenInformation(
            token.get(),
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            required_bytes,
            &mut required_bytes,
        )?;
    }

    let token_user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut sid_text = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text)?;
    }

    let sid = unsafe { sid_text.to_string() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid_text.0.cast())));
    }

    Ok(UserSid::from_windows_token(sid?))
}

async fn wait_for_dropped_watch(
    expected_watch_id: u64,
    dropped_watches: &mut mpsc::UnboundedReceiver<u64>,
) -> Result<(), BoxError> {
    let actual_watch_id = timeout(Duration::from_secs(2), dropped_watches.recv())
        .await?
        .ok_or("server stream drop notification channel closed")?;
    assert_eq!(actual_watch_id, expected_watch_id);

    Ok(())
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}
