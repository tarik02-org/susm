use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use tempfile::tempdir;
use windows::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
};
use windows::core::PCWSTR;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempdir()?;
    let destination = directory.path().join("runtime.susm-runtime.open");
    let staging = directory.path().join("runtime.susm-runtime.compact");
    write_synced(&destination, b"old checkpoint")?;
    write_synced(&staging, b"new checkpoint")?;

    let mut old_reader = File::open(&destination)?;
    replace(&destination, &staging)?;

    let mut old_content = String::new();
    old_reader.seek(SeekFrom::Start(0))?;
    old_reader.read_to_string(&mut old_content)?;
    if old_content != "old checkpoint" {
        return Err(format!("open reader changed view to {old_content:?}").into());
    }

    let new_content = std::fs::read_to_string(&destination)?;
    if new_content != "new checkpoint" {
        return Err(format!("new reader saw {new_content:?}").into());
    }

    let explicit = directory.path().join("explicit.susm-journal");
    let explicit_staging = directory.path().join("explicit.susm-journal.zst.tmp");
    write_synced(&explicit, b"uncompressed")?;
    write_synced(&explicit_staging, b"compressed")?;
    let share_mode = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;
    let mut explicit_reader = OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .open(&explicit)?;
    replace(&explicit, &explicit_staging)?;

    let mut explicit_old = String::new();
    explicit_reader.read_to_string(&mut explicit_old)?;
    if explicit_old != "uncompressed" {
        return Err(format!("explicit reader changed view to {explicit_old:?}").into());
    }

    println!("ok ReplaceFileW with Rust default reader sharing");
    println!("ok ReplaceFileW with explicit read/write/delete sharing");
    println!("ok old handle retains old bytes and new open sees replacement");
    Ok(())
}

fn write_synced(path: &Path, contents: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn replace(destination: &Path, staging: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let destination = wide_null(destination);
    let staging = wide_null(staging);
    unsafe {
        ReplaceFileW(
            PCWSTR(destination.as_ptr()),
            PCWSTR(staging.as_ptr()),
            PCWSTR::null(),
            REPLACEFILE_WRITE_THROUGH,
            None,
            None,
        )?;
    }
    Ok(())
}

fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}
