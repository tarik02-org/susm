use std::{env, error::Error, fs, path::PathBuf, process::Command};

use prost::Message;
use prost_types::FileDescriptorSet;

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest dir")?);
    let descriptor = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing output dir")?)
        .join("susm-descriptor.bin");

    println!("cargo:rerun-if-changed=buf.yaml");
    println!("cargo:rerun-if-changed=proto");
    let status = Command::new("buf")
        .arg("build")
        .arg(&root)
        .arg("--as-file-descriptor-set")
        .arg("--output")
        .arg(&descriptor)
        .status()?;
    if !status.success() {
        return Err(format!("buf build failed with {status}").into());
    }

    let bytes = fs::read(descriptor)?;
    tonic_prost_build::configure().compile_fds(FileDescriptorSet::decode(bytes.as_slice())?)?;
    Ok(())
}
