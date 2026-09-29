// Modified by RenCrow Switch Core, 2026-09-28: read-only agent roles (ROLE_TOOL_POLICY.md).
//! `rencrow_read_only` in an agent role file narrows that role's sandbox to read-only on top of
//! the permissions the child inherited, so an agent that follows an instruction embedded in data
//! it reads cannot change files. It only narrows: reads, read denials, and the network policy are
//! kept from the inherited profile.

use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::FileSystemSandboxPolicy;

/// Removes write access from an inherited permission profile.
pub(crate) fn read_only_profile(profile: &PermissionProfile) -> Result<PermissionProfile, String> {
    match profile {
        PermissionProfile::Disabled => Ok(PermissionProfile::read_only()),
        PermissionProfile::External { .. } => {
            Err("a read-only agent role cannot narrow an external sandbox".to_string())
        }
        PermissionProfile::Managed { .. } => {
            let enforcement = profile.enforcement();
            let (mut file_system, network) = profile.to_runtime_permissions();
            match file_system.kind {
                FileSystemSandboxKind::Unrestricted => {
                    file_system = FileSystemSandboxPolicy::read_only();
                }
                FileSystemSandboxKind::ExternalSandbox => {
                    return Err(
                        "a read-only agent role cannot narrow an external sandbox".to_string()
                    );
                }
                FileSystemSandboxKind::Restricted => {
                    for entry in &mut file_system.entries {
                        if entry.access == FileSystemAccessMode::Write {
                            entry.access = FileSystemAccessMode::Read;
                        }
                    }
                }
            }
            Ok(
                PermissionProfile::from_runtime_permissions_with_enforcement(
                    enforcement,
                    &file_system,
                    network,
                ),
            )
        }
    }
}

#[cfg(test)]
#[path = "rencrow_read_only_tests.rs"]
mod tests;
