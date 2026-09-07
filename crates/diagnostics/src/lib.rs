use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Map, Value, json};
use thiserror::Error;
use tracing_subscriber::fmt::MakeWriter;
use windows::{
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            SetFileSecurityW,
        },
    },
    core::PCWSTR,
};

const QUEUE_BYTES: usize = 1024 * 1024;
const QUEUE_MESSAGES: usize = 4096;
const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
const SEGMENT_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const RETENTION_BYTES: u64 = 256 * 1024 * 1024;
const RETENTION_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Copy)]
pub enum Component {
    Host,
    Controller,
    Supervisor,
}

impl Component {
    fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Controller => "controller",
            Self::Supervisor => "supervisor",
        }
    }
}

#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error("diagnostic storage failed: {0}")]
    Io(#[from] io::Error),
    #[error("diagnostic storage security failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("the process diagnostic subscriber is already configured")]
    SubscriberAlreadyConfigured,
}

pub struct Guard {
    queue: Arc<Queue>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Guard {
    pub fn shutdown(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        self.queue.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.finish();
    }
}

pub fn init(component: Component, directory: &Path) -> Result<Guard, DiagnosticsError> {
    fs::create_dir_all(directory)?;
    if matches!(component, Component::Host) {
        secure_host_tree(directory)?;
    }
    apply_retention(directory)?;
    let segment = Segment::open(directory)?;
    let (sender, receiver) = mpsc::sync_channel(QUEUE_MESSAGES);
    let queue = Arc::new(Queue {
        sender,
        queued_bytes: AtomicUsize::new(0),
        dropped: AtomicU64::new(0),
        shutdown: AtomicBool::new(false),
    });
    let worker_queue = queue.clone();
    let directory = directory.to_owned();
    let worker = thread::Builder::new()
        .name(format!("susm-{}-diagnostics", component.name()))
        .spawn(move || run_writer(component, directory, segment, receiver, worker_queue))?;
    let writer = DiagnosticWriter {
        queue: queue.clone(),
    };
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(writer)
        .finish();
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        queue.shutdown.store(true, Ordering::Release);
        let _ = worker.join();
        return Err(DiagnosticsError::SubscriberAlreadyConfigured);
    }
    Ok(Guard {
        queue,
        worker: Some(worker),
    })
}

fn secure_host_tree(directory: &Path) -> Result<(), DiagnosticsError> {
    set_host_dacl(directory)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            set_host_dacl(&entry.path())?;
        }
    }
    Ok(())
}

fn set_host_dacl(path: &Path) -> Result<(), DiagnosticsError> {
    use std::os::windows::ffi::OsStrExt as _;

    let sddl = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        .encode_utf16()
        .chain([0])
        .collect::<Vec<_>>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    let descriptor = LocalDescriptor(descriptor.0);
    let path = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    unsafe {
        SetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR(descriptor.0),
        )
        .ok()?;
    }
    Ok(())
}

struct LocalDescriptor(*mut core::ffi::c_void);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0)));
        }
    }
}

struct Queue {
    sender: mpsc::SyncSender<Vec<u8>>,
    queued_bytes: AtomicUsize,
    dropped: AtomicU64,
    shutdown: AtomicBool,
}

impl Queue {
    fn submit(&self, bytes: Vec<u8>) {
        if bytes.is_empty() || self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let length = bytes.len();
        if length > QUEUE_BYTES || !self.reserve(length) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.sender.try_send(bytes).is_err() {
            self.queued_bytes.fetch_sub(length, Ordering::AcqRel);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn reserve(&self, length: usize) -> bool {
        let mut queued = self.queued_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = queued.checked_add(length) else {
                return false;
            };
            if next > QUEUE_BYTES {
                return false;
            }
            match self.queued_bytes.compare_exchange_weak(
                queued,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => queued = actual,
            }
        }
    }
}

#[derive(Clone)]
struct DiagnosticWriter {
    queue: Arc<Queue>,
}

impl<'a> MakeWriter<'a> for DiagnosticWriter {
    type Writer = EventBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        EventBuffer {
            queue: self.queue.clone(),
            bytes: Vec::with_capacity(512),
        }
    }
}

struct EventBuffer {
    queue: Arc<Queue>,
    bytes: Vec<u8>,
}

impl Write for EventBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventBuffer {
    fn drop(&mut self) {
        self.queue.submit(std::mem::take(&mut self.bytes));
    }
}

fn run_writer(
    component: Component,
    directory: PathBuf,
    mut segment: Segment,
    receiver: mpsc::Receiver<Vec<u8>>,
    queue: Arc<Queue>,
) {
    loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(bytes) => {
                queue.queued_bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
                let dropped = queue.dropped.swap(0, Ordering::AcqRel);
                if dropped != 0 {
                    let record = dropped_record(component, dropped);
                    if write_record(&directory, &mut segment, &record).is_err() {
                        return;
                    }
                }
                if let Some(record) = normalize_record(component, &bytes)
                    && write_record(&directory, &mut segment, &record).is_err()
                {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if segment.flush().is_err() {
                    return;
                }
                if segment.should_rotate(0) && rotate(&directory, &mut segment).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if queue.shutdown.load(Ordering::Acquire) {
            while let Ok(bytes) = receiver.try_recv() {
                queue.queued_bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
                if let Some(record) = normalize_record(component, &bytes) {
                    let _ = write_record(&directory, &mut segment, &record);
                }
            }
            let dropped = queue.dropped.swap(0, Ordering::AcqRel);
            if dropped != 0 {
                let _ = write_record(
                    &directory,
                    &mut segment,
                    &dropped_record(component, dropped),
                );
            }
            break;
        }
    }
    let _ = segment.finish();
    let _ = apply_retention(&directory);
}

fn normalize_record(component: Component, bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(bytes).ok()?;
    let object = value.as_object_mut()?;
    object.insert(
        "component".to_owned(),
        Value::String(component.name().to_owned()),
    );
    object.insert("pid".to_owned(), Value::from(std::process::id()));
    let name = object
        .get("fields")
        .and_then(Value::as_object)
        .and_then(|fields| fields.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("event")
        .to_owned();
    object.insert("name".to_owned(), Value::String(name));
    serde_json::to_vec(&value).ok().map(|mut bytes| {
        bytes.push(b'\n');
        bytes
    })
}

fn dropped_record(component: Component, dropped: u64) -> Vec<u8> {
    let mut fields = Map::new();
    fields.insert("count".to_owned(), Value::from(dropped));
    let value = json!({
        "timestamp": jiff::Timestamp::now().to_string(),
        "level": "WARN",
        "component": component.name(),
        "pid": std::process::id(),
        "target": "susm_diagnostics",
        "name": "diagnostics_dropped",
        "fields": fields,
    });
    let mut bytes = serde_json::to_vec(&value).expect("fixed diagnostic record is serializable");
    bytes.push(b'\n');
    bytes
}

fn write_record(directory: &Path, segment: &mut Segment, record: &[u8]) -> io::Result<()> {
    if segment.should_rotate(record.len() as u64) {
        rotate(directory, segment)?;
    }
    segment.write(record)
}

fn rotate(directory: &Path, segment: &mut Segment) -> io::Result<()> {
    segment.finish()?;
    apply_retention(directory)?;
    *segment = Segment::open(directory)?;
    Ok(())
}

struct Segment {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    bytes: u64,
    opened: Instant,
}

impl Segment {
    fn open(directory: &Path) -> io::Result<Self> {
        let timestamp = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%S.%9fZ")
            .to_string();
        for sequence in 0..1000_u32 {
            let path = directory.join(format!(
                "{timestamp}-{:010}-{sequence:03}.jsonl.open",
                std::process::id()
            ));
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        writer: Some(BufWriter::new(file)),
                        bytes: 0,
                        opened: Instant::now(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "diagnostic segment namespace exhausted",
        ))
    }

    fn should_rotate(&self, next_bytes: u64) -> bool {
        self.bytes != 0
            && (self.bytes.saturating_add(next_bytes) > SEGMENT_BYTES
                || self.opened.elapsed() >= SEGMENT_AGE)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer
            .as_mut()
            .expect("open diagnostic segment has a writer")
            .write_all(bytes)?;
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .as_mut()
            .expect("open diagnostic segment has a writer")
            .flush()
    }

    fn finish(&mut self) -> io::Result<()> {
        let Some(mut writer) = self.writer.take() else {
            return Ok(());
        };
        if self.bytes == 0 {
            drop(writer);
            return fs::remove_file(&self.path);
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        fs::rename(&self.path, self.path.with_extension(""))
    }
}

struct FinalizedFile {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
    removed: bool,
}

fn apply_retention(directory: &Path) -> io::Result<()> {
    let mut files = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            Some(FinalizedFile {
                path,
                modified: metadata.modified().ok()?,
                bytes: metadata.len(),
                removed: false,
            })
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|file| file.modified);
    let now = SystemTime::now();
    for file in &mut files {
        if now.duration_since(file.modified).unwrap_or_default() > RETENTION_AGE {
            fs::remove_file(&file.path)?;
            file.removed = true;
        }
    }
    let mut retained = files
        .iter()
        .filter(|file| !file.removed)
        .map(|file| file.bytes)
        .sum::<u64>();
    for file in &mut files {
        if retained <= RETENTION_BYTES {
            break;
        }
        if !file.removed {
            fs::remove_file(&file.path)?;
            file.removed = true;
            retained = retained.saturating_sub(file.bytes);
        }
    }
    Ok(())
}
