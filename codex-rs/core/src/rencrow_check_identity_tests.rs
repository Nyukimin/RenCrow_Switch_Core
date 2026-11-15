use super::*;

#[test]
fn parses_literal_export_setup_and_preserves_all_explicit_environment() {
    let parsed = parse(
        "export PATH=/opt/bin:/usr/bin && export RUSTUP_TOOLCHAIN=1.95.0 && just test --locked -p codex-core --lib -E 'test(foo)'",
        Path::new("/workspace"),
        |words| words.starts_with(&["just".to_owned(), "test".to_owned()]),
    )
    .expect("literal setup before a supported direct check should parse");
    assert_eq!(parsed.identity.cwd, PathBuf::from("/workspace"));
    assert_eq!(parsed.identity.runner[0..2], ["just", "test"]);
    assert_eq!(
        parsed.identity.environment.get("PATH").unwrap(),
        "/opt/bin:/usr/bin"
    );
    assert_eq!(
        parsed.identity.environment.get("RUSTUP_TOOLCHAIN").unwrap(),
        "1.95.0"
    );
}

#[test]
fn startup_failure_retry_is_limited_to_the_observed_cargo_launch_repair() {
    let previous = CheckIdentity {
        cwd: PathBuf::from("/workspace"),
        cwd_reliable: true,
        runner: vec!["just".into(), "test".into(), "--lib".into()],
        environment: BTreeMap::new(),
    };
    let output = "RUST_MIN_STACK=16777216 NEXTEST_PROFILE=local cargo nextest run --no-fail-fast \"$@\"\ntest: cargo: command not found\nerror: recipe `test` failed on line 88 with exit code 127";
    assert!(is_cargo_not_found_startup_failure(
        &previous, 127, output, false
    ));
    assert!(!is_cargo_not_found_startup_failure(
        &previous, 127, output, true
    ));
    assert!(!is_cargo_not_found_startup_failure(
        &previous, 1, output, false
    ));
    assert!(!is_cargo_not_found_startup_failure(
        &previous,
        127,
        &format!("{output}\nextra output"),
        false
    ));
    assert!(!is_cargo_not_found_startup_failure(
        &previous,
        127,
        "test runner exited 127",
        false
    ));

    let mut current = previous.clone();
    current
        .environment
        .insert("PATH".into(), "/opt/bin:/usr/bin".into());
    assert!(is_cargo_not_found_retry(&previous, &current));
    current
        .environment
        .insert("RUSTUP_TOOLCHAIN".into(), "1.95.0".into());
    assert!(is_cargo_not_found_retry(&previous, &current));

    let mut changed = current.clone();
    changed.runner.push("-E test(other)".into());
    assert!(!is_cargo_not_found_retry(&previous, &changed));
    changed = current.clone();
    changed.cwd.push("elsewhere");
    assert!(!is_cargo_not_found_retry(&previous, &changed));
    changed = current.clone();
    changed.environment.insert("TEST_SKIP".into(), "1".into());
    assert!(!is_cargo_not_found_retry(&previous, &changed));
    changed = previous.clone();
    changed.environment.insert("PATH".into(), "/old/bin".into());
    changed
        .environment
        .insert("RUSTUP_TOOLCHAIN".into(), "old".into());
    let mut toolchain_changed = changed.clone();
    toolchain_changed
        .environment
        .insert("PATH".into(), "/new/bin".into());
    toolchain_changed
        .environment
        .insert("RUSTUP_TOOLCHAIN".into(), "new".into());
    assert!(!is_cargo_not_found_retry(&changed, &toolchain_changed));

    changed = previous.clone();
    changed.cwd_reliable = false;
    changed
        .environment
        .insert("PATH".into(), "/opt/bin:/usr/bin".into());
    assert!(!is_cargo_not_found_retry(&previous, &changed));
}
