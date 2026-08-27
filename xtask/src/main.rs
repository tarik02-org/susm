use std::{
    error::Error,
    fmt::Write as _,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};

const USER_BINARIES: [&str; 3] = ["susm.exe", "susmd.exe", "susm-supervisor.exe"];
const PACKAGES: [&str; 4] = [
    "susm-cli",
    "susm-controller",
    "susm-supervisor",
    "susm-host",
];

#[derive(Parser)]
#[command(name = "xtask")]
struct Arguments {
    #[command(subcommand)]
    command: Task,
}

#[derive(Subcommand)]
enum Task {
    Package {
        #[arg(long)]
        version: String,

        #[arg(long)]
        target: Option<String>,

        #[arg(long, default_value = "dist")]
        output: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    match Arguments::parse().command {
        Task::Package {
            version,
            target,
            output,
        } => package(&version, target.as_deref(), &output),
    }
}

fn package(version: &str, target: Option<&str>, output: &Path) -> Result<(), Box<dyn Error>> {
    if version != env!("CARGO_PKG_VERSION") {
        return Err(format!(
            "package version {version} does not match workspace version {}",
            env!("CARGO_PKG_VERSION")
        )
        .into());
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask must be inside the workspace")?;
    let target = target.unwrap_or(match std::env::consts::ARCH {
        "x86_64" => "x86_64-pc-windows-msvc",
        "aarch64" => "aarch64-pc-windows-msvc",
        architecture => return Err(format!("unsupported architecture: {architecture}").into()),
    });
    if !matches!(target, "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc") {
        return Err(format!("unsupported target: {target}").into());
    };

    let mut build = Command::new("cargo");
    build
        .current_dir(root)
        .args(["build", "--release", "--target", target]);
    for package in PACKAGES {
        build.args(["--package", package]);
    }
    if !build.status()?.success() {
        return Err("cargo build failed".into());
    }

    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        root.join(output)
    };
    fs::create_dir_all(&output)?;

    let name = format!("susm-{version}-{target}");
    let bundle = output.join(&name);
    let staging = output.join(format!(".{name}.{}.tmp", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }

    let result = stage_bundle(root, &staging, version, target);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    if bundle.exists() {
        fs::remove_dir_all(&bundle)?;
    }
    fs::rename(&staging, &bundle)?;
    println!("{}", bundle.display());
    Ok(())
}

fn stage_bundle(
    root: &Path,
    staging: &Path,
    version: &str,
    target: &str,
) -> Result<(), Box<dyn Error>> {
    let binaries = root.join("target").join(target).join("release");
    let user = staging.join("user");
    let host = staging.join("host");
    fs::create_dir_all(&user)?;
    fs::create_dir_all(&host)?;

    for name in USER_BINARIES {
        fs::copy(binaries.join(name), user.join(name))?;
    }
    fs::copy(binaries.join("susm-host.exe"), host.join("susm-host.exe"))?;

    let mut manifest = format!(
        "bundle_format = 1\nversion = \"{version}\"\ntarget = \"{target}\"\nprotocol_major = 1\ncontroller_schema_read_min = 1\ncontroller_schema_read_max = 2\ncontroller_schema_write = 2\nsupervisor_runtime_formats = [1]\n"
    );
    for name in USER_BINARIES {
        let path = user.join(name);
        write!(
            manifest,
            "\n[[files]]\npath = \"{name}\"\nsize = {}\nsha256 = \"{}\"\n",
            fs::metadata(&path)?.len(),
            digest_file(&path)?
        )?;
    }
    fs::write(user.join("manifest.toml"), manifest)?;
    Ok(())
}

fn digest_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(output)
}
