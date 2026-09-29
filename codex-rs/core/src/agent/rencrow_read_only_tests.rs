use super::*;
use codex_protocol::permissions::NetworkSandboxPolicy;
use pretty_assertions::assert_eq;

fn write_entries(profile: &PermissionProfile) -> usize {
    let (file_system, _) = profile.to_runtime_permissions();
    file_system
        .entries
        .iter()
        .filter(|entry| entry.access == FileSystemAccessMode::Write)
        .count()
}

#[test]
fn workspace_write_loses_only_its_write_access() {
    let workspace_write = PermissionProfile::workspace_write();
    assert!(write_entries(&workspace_write) > 0);

    let narrowed = read_only_profile(&workspace_write).expect("managed profile narrows");

    assert_eq!(write_entries(&narrowed), 0);
    let (before_fs, before_network) = workspace_write.to_runtime_permissions();
    let (after_fs, after_network) = narrowed.to_runtime_permissions();
    assert_eq!(after_network, before_network);
    assert_eq!(narrowed.enforcement(), workspace_write.enforcement());
    // Every path stays listed; only its access drops from write to read.
    assert_eq!(
        after_fs
            .entries
            .iter()
            .map(|entry| &entry.path)
            .collect::<Vec<_>>(),
        before_fs
            .entries
            .iter()
            .map(|entry| &entry.path)
            .collect::<Vec<_>>()
    );
}

#[test]
fn read_only_stays_read_only() {
    let read_only = PermissionProfile::read_only();

    assert_eq!(
        read_only_profile(&read_only).expect("read-only narrows to itself"),
        read_only
    );
}

#[test]
fn no_sandbox_becomes_the_built_in_read_only_profile() {
    assert_eq!(
        read_only_profile(&PermissionProfile::Disabled).expect("disabled narrows"),
        PermissionProfile::read_only()
    );
}

#[test]
fn external_sandbox_is_refused_instead_of_widened() {
    let external = PermissionProfile::External {
        network: NetworkSandboxPolicy::Restricted,
    };

    assert!(read_only_profile(&external).is_err());
}
