#![cfg(windows)]
#![forbid(unsafe_code)]

mod manager;
mod rpc;
mod storage;
mod supervisor_rpc;

pub use manager::{Manager, ManagerError, ManagerPaths};
pub use rpc::ControlRpc;
pub use storage::StorageError;
pub use supervisor_rpc::SupervisorRpc;
