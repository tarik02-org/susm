use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    mem::size_of,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use prost::Message;
use susm_protocol::{
    runtime::{Checkpoint, RuntimeFrame, RuntimeObservation, runtime_frame},
    supervisor::{ExecutionConfiguration, PolicyUpdate},
};
use tokio::sync::{mpsc, watch};

pub const OUTPUT_CHUNK_SIZE: usize = 64 * 1024;
pub const OUTPUT_QUEUE_CHUNKS: usize = 128;

#[derive(Clone, Copy, Debug)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub sequence: u64,
    pub timestamp_unix_ms: i64,
    pub bytes: Vec<u8>,
}

pub async fn run_writer(
    mut configuration: ExecutionConfiguration,
    mut chunks: mpsc::Receiver<OutputChunk>,
    mut policy: watch::Receiver<Option<PolicyUpdate>>,
) -> io::Result<()> {
    let mut next_segment = 0;
    let mut writer = if configuration.capture_logs {
        fs::create_dir_all(&configuration.log_directory)?;
        let writer = SegmentWriter::open(&configuration, next_segment)?;
        next_segment += 1;
        Some(writer)
    } else {
        None
    };
    let mut policy_open = true;
    let mut flush = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            chunk = chunks.recv() => {
                let Some(chunk) = chunk else {
                    break;
                };
                let Some(writer) = writer.as_mut() else {
                    continue;
                };
                let entry = encode_entry(&configuration, &chunk);
                if writer.should_rotate(entry.len() as u64, &configuration) {
                    writer.finish()?;
                    apply_retention(&configuration)?;
                    *writer = SegmentWriter::open(&configuration, next_segment)?;
                    next_segment += 1;
                }
                writer.write(&entry)?;
            }
            _ = flush.tick() => {
                if let Some(writer) = writer.as_mut() {
                    writer.flush()?;
                    if writer.should_rotate(0, &configuration) {
                        writer.finish()?;
                        apply_retention(&configuration)?;
                        *writer = SegmentWriter::open(&configuration, next_segment)?;
                        next_segment += 1;
                    }
                }
            }
            changed = policy.changed(), if policy_open => {
                if changed.is_err() {
                    policy_open = false;
                    continue;
                }
                let Some(update) = policy.borrow_and_update().clone() else {
                    continue;
                };
                apply_logging_policy(&mut configuration, &update);
                match (configuration.capture_logs, writer.as_mut()) {
                    (false, Some(active)) => {
                        active.finish()?;
                        writer = None;
                    }
                    (true, None) => {
                        fs::create_dir_all(&configuration.log_directory)?;
                        writer = Some(SegmentWriter::open(&configuration, next_segment)?);
                        next_segment += 1;
                    }
                    (true, Some(active)) if active.should_rotate(0, &configuration) => {
                        active.finish()?;
                        writer = Some(SegmentWriter::open(&configuration, next_segment)?);
                        next_segment += 1;
                    }
                    _ => {}
                }
                if configuration.capture_logs {
                    apply_retention(&configuration)?;
                }
            }
        }
    }
    if let Some(writer) = writer.as_mut() {
        writer.finish()?;
        apply_retention(&configuration)?;
    }
    Ok(())
}

fn apply_logging_policy(configuration: &mut ExecutionConfiguration, update: &PolicyUpdate) {
    configuration.capture_logs = update.capture_logs;
    configuration.segment_size = update.segment_size;
    configuration.segment_age_ms = update.segment_age_ms;
    configuration.retention_size = update.retention_size;
    configuration.retention_size_unlimited = update.retention_size_unlimited;
    configuration.retention_age_ms = update.retention_age_ms;
    configuration.retention_age_unlimited = update.retention_age_unlimited;
}

struct SegmentWriter {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    bytes: u64,
    opened: Instant,
}

impl SegmentWriter {
    fn open(configuration: &ExecutionConfiguration, segment: u32) -> io::Result<Self> {
        let timestamp = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%S.%9fZ")
            .to_string();
        let path = Path::new(&configuration.log_directory)
            .join(format!("{timestamp:020}-{segment:06}.susm-journal.open"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            path,
            writer: Some(BufWriter::new(file)),
            bytes: 0,
            opened: Instant::now(),
        })
    }

    fn should_rotate(&self, next_bytes: u64, configuration: &ExecutionConfiguration) -> bool {
        self.bytes != 0
            && (self.bytes.saturating_add(next_bytes) > configuration.segment_size
                || self.opened.elapsed() >= Duration::from_millis(configuration.segment_age_ms))
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer
            .as_mut()
            .expect("open segment has a writer")
            .write_all(bytes)?;
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .as_mut()
            .expect("open segment has a writer")
            .flush()
    }

    fn finish(&mut self) -> io::Result<()> {
        let Some(mut writer) = self.writer.take() else {
            return Ok(());
        };
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        let finalized = self.path.with_extension("");
        fs::rename(&self.path, &finalized)?;
        let compressed = appended_extension(&finalized, ".zst");
        let temporary = appended_extension(&finalized, &format!(".zst.{}.tmp", std::process::id()));
        let mut source = File::open(&finalized)?;
        let mut target = File::create(&temporary)?;
        zstd::stream::copy_encode(&mut source, &mut target, 3)?;
        target.sync_all()?;
        drop(target);
        drop(source);
        fs::rename(&temporary, &compressed)?;
        fs::remove_file(finalized)
    }
}

fn appended_extension(path: &Path, extension: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(extension);
    PathBuf::from(value)
}

fn encode_entry(configuration: &ExecutionConfiguration, chunk: &OutputChunk) -> Vec<u8> {
    let mut output = Vec::with_capacity(chunk.bytes.len() + 256);
    scalar(&mut output, "SUSM_FORMAT", "1");
    scalar(&mut output, "SUSM_RECORD", "output");
    scalar(
        &mut output,
        "SUSM_TIMESTAMP_UNIX_MS",
        &chunk.timestamp_unix_ms.to_string(),
    );
    scalar(&mut output, "SUSM_WORKLOAD", &configuration.workload_id);
    scalar(&mut output, "SUSM_EXECUTION", &configuration.execution_id);
    scalar(
        &mut output,
        "SUSM_ATTEMPT",
        &configuration.attempt.to_string(),
    );
    scalar(&mut output, "SUSM_SEQUENCE", &chunk.sequence.to_string());
    scalar(&mut output, "SUSM_STREAM", chunk.stream.name());
    output.extend_from_slice(b"MESSAGE\n");
    output.extend_from_slice(&(chunk.bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(&chunk.bytes);
    output.extend_from_slice(b"\n\n");
    output
}

fn scalar(output: &mut Vec<u8>, name: &str, value: &str) {
    output.extend_from_slice(name.as_bytes());
    output.push(b'=');
    output.extend_from_slice(value.as_bytes());
    output.push(b'\n');
}

fn apply_retention(configuration: &ExecutionConfiguration) -> io::Result<()> {
    let attempt = Path::new(&configuration.log_directory);
    let workload = attempt
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("log directory is outside a workload tree"))?;
    let mut files = Vec::new();
    collect_finalized(workload, &mut files)?;
    files.sort_by_key(|file| file.modified);
    let now = SystemTime::now();
    if !configuration.retention_age_unlimited {
        let age = Duration::from_millis(configuration.retention_age_ms);
        for file in &mut files {
            if now.duration_since(file.modified).unwrap_or_default() > age {
                fs::remove_file(&file.path)?;
                file.removed = true;
            }
        }
    }
    if !configuration.retention_size_unlimited {
        let mut bytes = files
            .iter()
            .filter(|file| !file.removed)
            .map(|file| file.bytes)
            .sum::<u64>();
        for file in &mut files {
            if bytes <= configuration.retention_size {
                break;
            }
            if !file.removed {
                fs::remove_file(&file.path)?;
                file.removed = true;
                bytes = bytes.saturating_sub(file.bytes);
            }
        }
    }
    Ok(())
}

struct FinalizedFile {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
    removed: bool,
}

fn collect_finalized(root: &Path, files: &mut Vec<FinalizedFile>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_finalized(&entry.path(), files)?;
        } else if entry
            .file_name()
            .to_string_lossy()
            .contains(".susm-journal")
        {
            files.push(FinalizedFile {
                path: entry.path(),
                modified: metadata.modified()?,
                bytes: metadata.len(),
                removed: false,
            });
        }
    }
    Ok(())
}

const MAX_RUNTIME_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct RuntimeJournal(Arc<Mutex<RuntimeJournalInner>>);

struct RuntimeJournalInner {
    path: PathBuf,
    file: Option<File>,
    frames: Vec<RuntimeFrame>,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeRecovery {
    pub checkpoint: Option<Checkpoint>,
    pub last_sequence: u64,
    pub committed_sequence: u64,
}

impl RuntimeJournal {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let frames = read_runtime_frames(path)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self(Arc::new(Mutex::new(RuntimeJournalInner {
            path: path.to_owned(),
            file: Some(file),
            frames,
        }))))
    }

    pub fn recovery(&self) -> RuntimeRecovery {
        let inner = self.0.lock().expect("runtime journal lock poisoned");
        let checkpoint = inner.frames.iter().rev().find_map(|frame| {
            if let Some(runtime_frame::Record::Checkpoint(checkpoint)) = &frame.record {
                Some(checkpoint.clone())
            } else {
                None
            }
        });
        let last_sequence = inner
            .frames
            .iter()
            .filter_map(|frame| match &frame.record {
                Some(runtime_frame::Record::Observation(observation)) => Some(observation.sequence),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let committed_sequence = inner
            .frames
            .iter()
            .filter_map(|frame| match &frame.record {
                Some(runtime_frame::Record::Acknowledgement(acknowledgement)) => {
                    Some(acknowledgement.committed_sequence)
                }
                Some(runtime_frame::Record::Checkpoint(checkpoint)) => {
                    Some(checkpoint.committed_sequence)
                }
                _ => None,
            })
            .max()
            .unwrap_or(0);
        RuntimeRecovery {
            checkpoint,
            last_sequence,
            committed_sequence,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.0
            .lock()
            .expect("runtime journal lock poisoned")
            .path
            .clone()
    }

    pub fn append_checkpoint(&self, checkpoint: Checkpoint) -> io::Result<()> {
        self.append(RuntimeFrame {
            format: 1,
            record: Some(runtime_frame::Record::Checkpoint(checkpoint)),
        })
    }

    pub fn append_observation(&self, observation: RuntimeObservation) -> io::Result<()> {
        self.append(RuntimeFrame {
            format: 1,
            record: Some(runtime_frame::Record::Observation(observation)),
        })
    }

    pub fn acknowledge(&self, committed_sequence: u64) -> io::Result<()> {
        self.append(RuntimeFrame {
            format: 1,
            record: Some(runtime_frame::Record::Acknowledgement(
                susm_protocol::runtime::Acknowledgement { committed_sequence },
            )),
        })
    }

    pub fn observations_after(&self, sequence: u64) -> Vec<RuntimeObservation> {
        self.0
            .lock()
            .expect("runtime journal lock poisoned")
            .frames
            .iter()
            .filter_map(|frame| match &frame.record {
                Some(runtime_frame::Record::Observation(observation))
                    if observation.sequence > sequence =>
                {
                    Some(observation.clone())
                }
                _ => None,
            })
            .collect()
    }

    pub fn finalize(&self) -> io::Result<()> {
        let mut inner = self.0.lock().expect("runtime journal lock poisoned");
        let Some(file) = inner.file.take() else {
            return Ok(());
        };
        file.sync_all()?;
        drop(file);
        let finalized = inner.path.with_extension("");
        fs::rename(&inner.path, finalized)
    }

    fn append(&self, frame: RuntimeFrame) -> io::Result<()> {
        let payload = frame.encode_to_vec();
        if payload.len() > MAX_RUNTIME_FRAME_BYTES {
            return Err(io::Error::other("runtime journal frame exceeds 4 MiB"));
        }
        let length = u32::try_from(payload.len())
            .map_err(|_| io::Error::other("runtime journal frame length overflowed"))?;
        let mut inner = self.0.lock().expect("runtime journal lock poisoned");
        let file = inner
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("runtime journal is finalized"))?;
        file.write_all(&length.to_le_bytes())?;
        file.write_all(&payload)?;
        file.write_all(&crc32c::crc32c(&payload).to_le_bytes())?;
        file.sync_data()?;
        inner.frames.push(frame);
        Ok(())
    }
}

fn read_runtime_frames(path: &Path) -> io::Result<Vec<RuntimeFrame>> {
    let mut bytes = Vec::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_end(&mut bytes)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let mut frames = Vec::new();
    let mut offset = 0;
    while bytes.len().saturating_sub(offset) >= size_of::<u32>() {
        let length = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("runtime frame length has four bytes"),
        ) as usize;
        if length > MAX_RUNTIME_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime journal frame exceeds 4 MiB",
            ));
        }
        let payload_start = offset + 4;
        let Some(checksum_start) = payload_start.checked_add(length) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime journal frame length overflowed",
            ));
        };
        let Some(frame_end) = checksum_start.checked_add(4) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime journal checksum offset overflowed",
            ));
        };
        if frame_end > bytes.len() {
            break;
        }
        let payload = &bytes[payload_start..checksum_start];
        let expected = u32::from_le_bytes(
            bytes[checksum_start..frame_end]
                .try_into()
                .expect("runtime checksum has four bytes"),
        );
        if crc32c::crc32c(payload) != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime journal checksum mismatch",
            ));
        }
        let frame = RuntimeFrame::decode(payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if frame.format != 1 || frame.record.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported runtime journal frame",
            ));
        }
        frames.push(frame);
        offset = frame_end;
    }
    Ok(frames)
}
