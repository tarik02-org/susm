#![cfg(windows)]

pub mod pipe;
pub mod session;

pub mod control {
    tonic::include_proto!("susm.control.v1");
}

pub mod supervisor {
    tonic::include_proto!("susm.supervisor.v1");
}

pub mod host {
    tonic::include_proto!("susm.host.v1");
}

pub mod runtime {
    tonic::include_proto!("susm.runtime.v1");
}

pub const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
