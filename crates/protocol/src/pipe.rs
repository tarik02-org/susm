use std::{
    fmt::{self, Display, Formatter},
    io,
    os::windows::io::AsRawHandle,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use hyper_util::rt::TokioIo;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
    },
    sync::{mpsc, watch},
    task::JoinHandle,
    time::sleep,
};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::transport::{Channel, Endpoint, server::Connected};
use tower::service_fn;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, E_INVALIDARG, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, HLOCAL,
            LocalFree,
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
            Pipes::{GetNamedPipeClientProcessId, ImpersonateNamedPipeClient},
            Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken},
        },
    },
    core::{PCWSTR, PWSTR},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UserSid(Arc<str>);

impl UserSid {
    pub fn parse_windows(value: String) -> Result<Self, InvalidSid> {
        if !value.starts_with("S-") || value.contains(['\\', '/', '\0']) {
            return Err(InvalidSid);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for UserSid {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSid;

impl Display for InvalidSid {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Windows SID")
    }
}

impl std::error::Error for InvalidSid {}

#[derive(Clone, Debug)]
pub struct CallerIdentity {
    pub sid: UserSid,
    pub process_id: u32,
}

pub struct AuthenticatedPipe {
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

pub struct PipeIncoming {
    incoming: ReceiverStream<Result<AuthenticatedPipe, io::Error>>,
    shutdown: watch::Sender<bool>,
    listener: Option<JoinHandle<io::Result<()>>>,
}

impl PipeIncoming {
    pub fn bind(pipe_name: Arc<str>, owning_sid: UserSid) -> Self {
        Self::bind_with_access(pipe_name, PipeAccess::User(owning_sid))
    }

    pub fn bind_authenticated_users(pipe_name: Arc<str>) -> Self {
        Self::bind_with_access(pipe_name, PipeAccess::AuthenticatedUsers)
    }

    fn bind_with_access(pipe_name: Arc<str>, access: PipeAccess) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel(16);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let listener = tokio::spawn(run_listener(pipe_name, access, incoming_tx, shutdown_rx));
        Self {
            incoming: ReceiverStream::new(incoming_rx),
            shutdown,
            listener: Some(listener),
        }
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        let _ = self.shutdown.send(true);
        if let Some(listener) = self.listener.take() {
            listener.await.map_err(io::Error::other)??;
        }
        Ok(())
    }
}

impl Stream for PipeIncoming {
    type Item = Result<AuthenticatedPipe, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.incoming).poll_next(context)
    }
}

impl Drop for PipeIncoming {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

pub fn control_pipe_name(sid: &UserSid, manager_session_id: &str) -> Arc<str> {
    format!(
        r"\\.\pipe\susm\users\{}\sessions\{}\control\v1",
        sid.as_str(),
        session_name(manager_session_id)
    )
    .into()
}

pub fn supervisor_pipe_name(sid: &UserSid, manager_session_id: &str) -> Arc<str> {
    format!(
        r"\\.\pipe\susm\users\{}\sessions\{}\supervisors\v1",
        sid.as_str(),
        session_name(manager_session_id)
    )
    .into()
}

pub fn host_pipe_name() -> Arc<str> {
    r"\\.\pipe\susm\host\v1".into()
}

fn session_name(manager_session_id: &str) -> &str {
    if manager_session_id.is_empty() {
        "standalone"
    } else {
        manager_session_id
    }
}

pub async fn connect(pipe_name: Arc<str>) -> Result<Channel, tonic::transport::Error> {
    Endpoint::from_static("http://susm.local")
        .connect_with_connector(service_fn(move |_| {
            let pipe_name = pipe_name.clone();
            async move { open_client(pipe_name).await.map(TokioIo::new) }
        }))
        .await
}

#[derive(Debug, Error)]
pub enum PipeConnectError {
    #[error("timed out waiting for the SUSM named pipe")]
    TimedOut,
    #[error("named-pipe transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),
}

pub async fn connect_for(
    pipe_name: Arc<str>,
    maximum_wait: Duration,
) -> Result<Channel, PipeConnectError> {
    tokio::time::timeout(maximum_wait, connect(pipe_name))
        .await
        .map_err(|_| PipeConnectError::TimedOut)?
        .map_err(Into::into)
}

pub fn current_user_sid() -> windows::core::Result<UserSid> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
    }
    token_sid(&OwnedHandle(token))
}

async fn run_listener(
    pipe_name: Arc<str>,
    access: PipeAccess,
    incoming: mpsc::Sender<Result<AuthenticatedPipe, io::Error>>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut first_instance = true;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let pipe = create_server(&pipe_name, first_instance, &access)?;
        first_instance = false;
        tokio::select! {
            result = pipe.connect() => result?,
            result = shutdown.changed() => {
                result.map_err(|_| io::Error::other("pipe shutdown sender dropped"))?;
                return Ok(());
            }
        }
        let caller = authenticate_client(&pipe).map_err(io::Error::other)?;
        if let PipeAccess::User(owning_sid) = &access
            && caller.sid != *owning_sid
        {
            continue;
        }
        if incoming
            .send(Ok(AuthenticatedPipe { pipe, caller }))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
}

fn create_server(
    pipe_name: &str,
    first_instance: bool,
    access: &PipeAccess,
) -> io::Result<NamedPipeServer> {
    let sddl = match access {
        PipeAccess::User(owning_sid) => {
            format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", owning_sid.as_str())
        }
        PipeAccess::AuthenticatedUsers => "D:P(A;;GA;;;SY)(A;;GRGW;;;AU)".to_owned(),
    };
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
            .max_instances(32)
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

#[derive(Clone)]
enum PipeAccess {
    User(UserSid),
    AuthenticatedUsers,
}

async fn open_client(pipe_name: Arc<str>) -> io::Result<NamedPipeClient> {
    let mut delay = Duration::from_millis(20);
    loop {
        match ClientOptions::new()
            .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
            .open(pipe_name.as_ref())
        {
            Ok(client) => return Ok(client),
            Err(error)
                if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND.0 as i32)
                    || error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) =>
            {
                sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    }
}

fn authenticate_client(pipe: &NamedPipeServer) -> windows::core::Result<CallerIdentity> {
    let handle = HANDLE(pipe.as_raw_handle());
    let mut process_id = 0;
    unsafe {
        GetNamedPipeClientProcessId(handle, &mut process_id)?;
    }
    let impersonation = Impersonation::begin(handle)?;
    let mut token = HANDLE::default();
    unsafe {
        OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token)?;
    }
    let sid = token_sid(&OwnedHandle(token));
    impersonation.finish()?;
    Ok(CallerIdentity {
        sid: sid?,
        process_id,
    })
}

fn token_sid(token: &OwnedHandle) -> windows::core::Result<UserSid> {
    let mut required = 0;
    unsafe {
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut required);
    }
    if required == 0 {
        return Err(windows::core::Error::from_thread());
    }
    let mut storage = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            required,
            &mut required,
        )?;
    }
    let token_user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut sid_text = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text)?;
    }
    let result = unsafe { sid_text.to_string() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid_text.0.cast())));
    }
    UserSid::parse_windows(result?)
        .map_err(|error| windows::core::Error::new(E_INVALIDARG, error.to_string()))
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Impersonation(bool);

impl Impersonation {
    fn begin(pipe: HANDLE) -> windows::core::Result<Self> {
        unsafe {
            ImpersonateNamedPipeClient(pipe)?;
        }
        Ok(Self(true))
    }

    fn finish(mut self) -> windows::core::Result<()> {
        unsafe {
            RevertToSelf()?;
        }
        self.0 = false;
        Ok(())
    }
}

impl Drop for Impersonation {
    fn drop(&mut self) {
        if self.0 && unsafe { RevertToSelf() }.is_err() {
            std::process::abort();
        }
    }
}
