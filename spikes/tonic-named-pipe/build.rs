use std::{env, error::Error, fs, path::PathBuf, process::Command};

use prost::Message;
use prost_types::FileDescriptorSet;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or("CARGO_MANIFEST_DIR must be set when building the Tonic named-pipe spike")?,
    );
    let descriptor_path = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or("OUT_DIR must be set when building the Tonic named-pipe spike")?,
    )
    .join("spike-descriptor.bin");

    println!("cargo:rerun-if-changed=buf.yaml");
    println!("cargo:rerun-if-changed=proto/susm/spike/v1/spike.proto");

    let status = Command::new("buf")
        .arg("build")
        .arg(&manifest_dir)
        .arg("--as-file-descriptor-set")
        .arg("--output")
        .arg(&descriptor_path)
        .status()?;

    if !status.success() {
        return Err(format!("buf build failed with {status}").into());
    }

    let descriptor = fs::read(descriptor_path)?;
    let descriptor_set = FileDescriptorSet::decode(descriptor.as_slice())?;

    tonic_prost_build::configure().compile_fds(descriptor_set)?;

    Ok(())
}
