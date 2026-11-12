use super::is_obvious_shell_edit;

#[test]
fn blocks_obvious_file_edit_commands() {
    for command in [
        "cp fixture.py source.py",
        "mv generated.py source.py",
        "env MODE=edit cp fixture.py source.py",
        "cat <<'EOF' > source.py\nVALUE = 1\nEOF",
        "cat <<'EOF' >> source.py\nVALUE = 1\nEOF",
        "python3 -c 'from pathlib import Path; Path(\"source.py\").write_text(\"VALUE = 1\")'",
        "python3 -c 'from pathlib import Path; Path(\"source.py\").write_bytes(b\"x\")'",
        "python3 -c 'open(\"source.py\", \"w\").write(\"VALUE = 1\")'",
        "python3 -c 'open(\"source.py\", mode=\"a\").write(\"VALUE = 1\")'",
        "python3 -c 'json.dump({}, open(\"source.json\", \"w\"))'",
        "python3 - <<'PY'\nfrom pathlib import Path\nPath('source.py').write_text('VALUE = 1')\nPY",
        r#"cd /repo && python3 -c "
import json, sys
with open('fixture.json') as f:
    data = json.load(f)
with open('fixture.json', 'w') as f:
    json.dump(data, f)
    f.write('\\n')
""#,
        "printf 'VALUE = 1\\n' > source.py",
    ] {
        assert!(
            is_obvious_shell_edit(command),
            "command was allowed: {command}"
        );
    }
}

#[test]
fn preserves_checks_reads_and_quoted_or_unknown_text() {
    for command in [
        "go test ./...",
        "GOCACHE=/tmp/cache go build ./...",
        "bash scripts/e2e-static.sh",
        "rg -n 'write_text|cp fixture' src",
        "cat logfile.txt | grep PASS",
        "go test ./... 2>&1",
        "echo 'python3 -c \\\"Path(\'x\').write_text(\'x\')\\\"'",
        "echo 'python3 -c Path.write_text()'",
        "echo \"python3 -c 'open(\\\"source.py\\\", \\\"w\\\")'\"",
        "python3 -c 'print(\"open(\\\"source.py\\\", \\\"w\\\")\")'",
        "rg -n 'open(\"source.py\", \"w\")' src",
        "python3 -c 'print(\".write_text(\")'",
        "python3 -c 'open(\"source.py\", \"r\").read()'",
        "python3 -c 'open(filename, mode=WRITE_MODE).write(data)'",
        "unsupported-shell-syntax <(echo safe)",
    ] {
        assert!(
            !is_obvious_shell_edit(command),
            "command was blocked: {command}"
        );
    }
}

#[test]
fn allows_only_unambiguous_dev_null_output_redirects() {
    for command in [
        "cd /Users/yukimi/RenCrow/RenCrow_StoryPlayer && ls testfixtures/semantic/ 2>/dev/null || echo \"no testfixtures/semantic dir\"",
        "printf data >/dev/null",
        "printf data >> /dev/null",
        "printf data 2>/dev/null",
        "printf data &>/dev/null",
        "printf data > /dev/null 2>&1",
        "printf data 2>&1",
    ] {
        assert!(
            !is_obvious_shell_edit(command),
            "command was blocked: {command}"
        );
    }

    for command in [
        "printf data > output.txt",
        "printf data >> output.txt",
        "printf data >/dev/null.txt",
        "printf data >/dev/null/child",
        "printf data >/dev/null$SUFFIX",
        "printf data >/dev/null$(suffix)",
        "printf data >/dev/null > output.txt",
        "printf data > /dev/null >> output.txt",
    ] {
        assert!(
            is_obvious_shell_edit(command),
            "command was allowed: {command}"
        );
    }
}
