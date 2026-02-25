pub mod config;
pub mod demarch;
pub mod ipc;
pub mod runtime;

pub use config::{IntercomConfig, load_config};
pub use demarch::{
    DemarchAdapter, DemarchCommandPlan, DemarchResponse, DemarchStatus, ReadOperation,
    WriteOperation,
};
pub use ipc::{IpcGroupContext, IpcMessage, IpcQuery, IpcQueryResponse, IpcTask};
pub use runtime::RuntimeKind;
