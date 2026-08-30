//! SandboxProvider：进程沙箱（借鉴 codex 的 SandboxPolicy enum）。

use std::path::PathBuf;
use tokio::process::Command;

/// 沙箱策略（借鉴 codex-rs）
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SandboxPolicy {
    /// 无限制（用户显式选择）
    DangerFullAccess,

    /// 只读文件系统；网络可选
    ReadOnly { network_access: bool },

    /// 工作区可写 + 指定额外可写根；网络可选
    WorkspaceWrite {
        writable_roots: Vec<PathBuf>,
        network_access: bool,
    },

    /// 外部沙箱（容器/VM 由实现方决定）
    ExternalSandbox { network_access: bool },
}

impl SandboxPolicy {
    pub fn workspace_write() -> Self {
        SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: false,
        }
    }
}

/// 平台沙箱机制类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SandboxType {
    None,
    MacosSeatbelt,
    LinuxSeccomp,
    WindowsRestrictedToken,
}

pub trait SandboxProvider: Send + Sync {
    fn policy(&self) -> SandboxPolicy;
    fn sandbox_type(&self) -> SandboxType;
    /// 对即将执行的命令施加沙箱约束（原地修改）
    fn apply(&self, command: &mut Command) -> Result<(), crate::error::YourAiError>;
    fn is_active(&self) -> bool;
}
