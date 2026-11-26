"""Test-owned bridge for the C29 Python owner -> native ThreadManager process path."""

import json
import hashlib
import fcntl
import os
from pathlib import Path
import pty
import select
import shlex
import struct
import subprocess
import sys
import termios
import tomllib
import time


def terminate_owned_process(process, grace_seconds=5):
    sent_termination = False
    if process.poll() is None:
        process.terminate()
        sent_termination = True
        try:
            process.wait(timeout=grace_seconds)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=grace_seconds)
    else:
        process.wait()
    if process.poll() is None:
        raise RuntimeError(f"test-owned process {process.pid} was not reaped")
    return sent_termination


def start_test_group_process(expected_group_id, *args, **kwargs):
    if os.getpgrp() != expected_group_id:
        raise RuntimeError("Python bridge left the verified nextest test process group")
    kwargs["start_new_session"] = False
    process = subprocess.Popen(*args, **kwargs)
    try:
        process_group_id = os.getpgid(process.pid)
    except OSError as error:
        terminate_owned_process(process)
        raise RuntimeError(
            f"could not verify test-owned child {process.pid} process group"
        ) from error
    if process_group_id != expected_group_id:
        terminate_owned_process(process)
        raise RuntimeError(
            f"test-owned child {process.pid} escaped nextest process group "
            f"{expected_group_id} into {process_group_id}"
        )
    return process, process_group_id


tools_root = Path(os.environ["RENCROW_TOOLS_ROOT"])
supervisor_root = tools_root / "tools/observability/ren2_supervisor"
handoff_source = supervisor_root / "parent_handoff.py"
expected_handoff_sha256 = os.environ["RENCROW_PARENT_HANDOFF_SHA256"]
actual_handoff_sha256 = hashlib.sha256(handoff_source.read_bytes()).hexdigest()
if actual_handoff_sha256 != expected_handoff_sha256:
    raise RuntimeError(
        "archived Tools parent_handoff.py hash differs from the pinned test source"
    )
tools_commit = os.environ["RENCROW_TOOLS_COMMIT"]
sys.path.insert(0, str(supervisor_root))

import parent_handoff
import supervise


class Host:
    def __init__(self, config):
        self.config = config
        self.ROOT = Path(config["cwd"])
        self.BASE = Path(config["base"])
        self.HOME = Path(config["home"])
        self.TMUX = "test-owned-tmux-adapter"
        self.pid = os.getpid()
        self.test_process_group_id = int(config["test_process_group_id"])
        self.source = None
        self.source_process_group_id = None
        self.child = None
        self.successor_cli_process_group_id = None
        self.child_master = None
        self.continuation_paste_pending = False
        self.continuation_submit_count = 0
        self.input_delivery_sequence = []
        self.cli_auth_file_store = False
        self.cli_auth_dummy_key_only = False
        self.cli_api_key_environment_absent = False
        self.cli_workspace_trusted = False
        self.cli_process_proof = None
        self.task_started_baseline = None
        self.child_log_path = self.BASE / "successor-cli-pty.log"
        self.child_log = open(self.child_log_path, "wb")
        self.duplicate_log = open(self.BASE / "duplicate-start.log", "wb")
        self.launch_count = 0
        self.continuation_count = 0
        self.exit_keys = []
        self.cleanup_signal_targets = []
        self.settings = config["settings"]
        self.source, self.source_process_group_id = start_test_group_process(
            self.test_process_group_id,
            [sys.executable, "-c", "import time; time.sleep(120)"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": str(self.HOME),
                "PYTHONDONTWRITEBYTECODE": "1",
            },
        )

    def source_snapshot(self):
        live = self.source.poll() is None
        snapshot = {
            "thread": self.config["source_thread"],
            "rollout": self.config["source_rollout"],
            "rollout_mtime": self.config["rollout_mtime"],
            "rollout_error": None,
            "turn_observation": {
                "turn_id": self.config["source_turn"],
                "active": False,
                "tool_running": False,
                "compacting": False,
                "completion_kind": "task_complete",
            },
            "pending_children": [],
            "child_rollout_errors": [],
            "child_receipts": {"unknown": []},
            "target_processes": (
                [
                    {
                        "pid": self.source.pid,
                        "command": f"/test/rencrow-switch-core resume {self.config['source_thread']} --rencrow-input-author automation",
                    }
                ]
                if live
                else []
            ),
            "processes": (
                [
                    f"{self.source.pid} {self.pid} 00:01 /test/rencrow-switch-core resume {self.config['source_thread']} --rencrow-input-author automation"
                ]
                if live
                else []
            ),
            "process_observation": {"returncode": 0, "monitored_cli_processes": []},
        }
        return snapshot

    def successor_snapshot(self):
        return {
            "thread": self.config["successor_thread"],
            "rollout": self.successor_rollout(),
            "rollout_mtime": 1,
            "rollout_error": None,
            "turn_observation": {
                "turn_id": self.config["source_turn"],
                "active": False,
                "tool_running": False,
                "compacting": False,
            },
            "pending_children": [],
            "child_rollout_errors": [],
            "child_receipts": {"unknown": []},
            "target_processes": [],
            "processes": [],
            "process_observation": {"returncode": 0, "monitored_cli_processes": []},
        }

    def successor_rollout(self):
        sessions = Path(self.config["home"]) / "sessions"
        matches = list(
            sessions.rglob("*" + self.config["successor_thread"] + "*.jsonl")
        )
        if len(matches) != 1:
            raise RuntimeError(
                f"expected exactly one native successor rollout, found {len(matches)}"
            )
        return str(matches[0])

    def require_host_action_enabled(self):
        if (self.BASE / "PAUSED").exists():
            raise RuntimeError("test-owned host paused")

    def validate_writer_observation(self, snap, settings):
        observation = snap.get(
            "process_observation", {"returncode": 0, "monitored_cli_processes": []}
        )
        if (
            observation.get("returncode") != 0
            or observation.get("monitored_cli_processes") != []
        ):
            raise RuntimeError("test-owned process observation is not quiescent")

    @staticmethod
    def turn_was_user_aborted(turn):
        return turn.get("completion_kind") == "turn_aborted"

    def active_task(self):
        return json.loads((self.BASE / "completion-contract.json").read_text())[
            "active_task"
        ]

    def target_process_pid(self, snap):
        if snap.get("thread") == self.config["source_thread"]:
            return self.source.pid
        if self.child is None:
            raise RuntimeError("native successor process is absent")
        return self.child.pid

    def target_input_author(self, snap, pid):
        return supervise.target_input_author(snap, pid)

    def has_executing_subprocesses(self, pids):
        return False

    def snapshot(self):
        self.settings = self.target_settings()
        if self.settings["thread"] == self.config["successor_thread"]:
            snap = self.successor_snapshot()
            if self.child is None or self.child.poll() is not None:
                raise RuntimeError("test-owned CLI successor process is not alive")
            self.drain_cli_output(0.0)
            command, parent_pid = self.observe_cli_process()
            snap.update(
                {
                    "pane": f"%test {self.child.pid} codex-test-child",
                    "target_processes": [{"pid": self.child.pid, "command": command}],
                    "processes": [f"{self.child.pid} {parent_pid} 00:01 {command}"],
                    "process_tree": [{"pid": self.child.pid, "ppid": parent_pid}],
                }
            )
            return snap
        return self.source_snapshot()

    def observe_cli_process(self):
        result = subprocess.run(
            [
                "/bin/ps",
                "-ww",
                "-o",
                "ppid=",
                "-o",
                "command=",
                "-p",
                str(self.child.pid),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
        if result.returncode != 0:
            raise RuntimeError(
                "could not observe the test-owned CLI process argv: " + result.stderr
            )
        fields = result.stdout.strip().split(None, 1)
        if len(fields) != 2:
            raise RuntimeError(
                "process argv observation did not return a parent PID and command"
            )
        parent_pid, command = fields
        expected = [
            self.config["cli_binary"],
            "--rencrow-input-author",
            "automation",
            "rencrow_parent_handoff_file=",
            self.config["record_file"],
        ]
        if any(value not in command for value in expected):
            raise RuntimeError(
                "observed CLI argv does not contain the exact test-owned launch contract"
            )
        if parent_pid != str(self.pid):
            raise RuntimeError(
                "CLI process is not a direct child of the test-owned host"
            )
        self.cli_process_proof = {
            "pid": self.child.pid,
            "parent_pid": int(parent_pid),
            "argv_observed_by_ps": command,
            "binary": self.config["cli_binary"],
            "binary_sha256": self.config["cli_binary_sha256"],
            "record_file": self.config["record_file"],
            "input_author": "automation",
        }
        (self.BASE / "cli-process-proof.json").write_text(
            json.dumps(self.cli_process_proof, ensure_ascii=False, indent=2)
        )
        return command, int(parent_pid)

    def target_settings(self):
        self.settings = json.loads((self.BASE / "settings.json").read_text())
        return self.settings

    def stop_owned_process(self, process):
        if terminate_owned_process(process):
            self.cleanup_signal_targets.append(process.pid)

    def save(self, path, value):
        Path(path).write_text(json.dumps(value, ensure_ascii=False))

    def command(self, args):
        if args[1:2] == ["display-message"]:
            return subprocess.CompletedProcess(args, 0, "zsh\n", "")
        if args[1:2] == ["send-keys"]:
            key = args[-1]
            self.exit_keys.append(key)
            if key == "C-c" and self.exit_keys.count("C-c") == 2:
                self.stop_owned_process(self.source)
            if key == "C-u":
                active_thread = self.target_settings()["thread"]
                if active_thread == self.config["source_thread"]:
                    if self.exit_keys != ["Escape", "C-u"]:
                        raise RuntimeError("source exit sequence is out of order")
                elif (
                    active_thread == self.config["successor_thread"]
                    and self.exit_keys.count("C-u") != 2
                ):
                    raise RuntimeError("successor input clear is out of order")
                elif active_thread not in [
                    self.config["source_thread"],
                    self.config["successor_thread"],
                ]:
                    raise RuntimeError("input clear targets an unknown owner")
            return subprocess.CompletedProcess(args, 0, "", "")
        raise RuntimeError("unexpected test-owned command: " + repr(args))

    def send(self, message):
        argv = shlex.split(message)
        handoff_values = []
        for index, argument in enumerate(argv):
            if argument == "-c" and index + 1 < len(argv):
                setting = argv[index + 1]
                if setting.startswith("rencrow_parent_handoff_file="):
                    handoff_values.append(self.launcher_toml_value(setting))
        is_new_parent = "--new" in argv or any(
            argument.startswith("rencrow_parent_handoff_file=") for argument in argv
        )
        if (
            self.launch_count == 0
            and self.continuation_count == 0
            and not is_new_parent
        ):
            raise RuntimeError(
                "first owner send did not contain a native parent launcher request"
            )
        if is_new_parent:
            if "--new" not in argv or len(handoff_values) != 1:
                raise RuntimeError("launcher handoff options are missing or ambiguous")
            requested_record = str(Path(handoff_values[0]).resolve(strict=True))
            expected_record = str(Path(self.config["record_file"]).resolve(strict=True))
            if requested_record != expected_record:
                raise RuntimeError(
                    "launcher did not pass the exact verified handoff record"
                )
            if "--rencrow-input-author" not in argv or "automation" not in argv:
                raise RuntimeError(
                    "launcher input author is not the required automation identity"
                )
            self.launch_count += 1
            if self.launch_count != 1:
                raise RuntimeError("CLI successor launch was replayed")
            child_env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": str(self.HOME),
                "CODEX_HOME": str(self.HOME),
                "NO_PROXY": "127.0.0.1,localhost",
                "no_proxy": "127.0.0.1,localhost",
                "PYTHONDONTWRITEBYTECODE": "1",
            }
            auth = json.loads((self.HOME / "auth.json").read_text())
            with (self.HOME / "config.toml").open("rb") as config_stream:
                cli_config = tomllib.load(config_stream)
            project = cli_config.get("projects", {}).get(
                self.config["trusted_workspace_key"], {}
            )
            self.cli_auth_file_store = (
                cli_config.get("cli_auth_credentials_store") == "file"
            )
            self.cli_auth_dummy_key_only = (
                auth.get("OPENAI_API_KEY") == "c29-test-only-dummy-key"
                and auth.get("tokens") is None
                and auth.get("last_refresh") is None
            )
            self.cli_api_key_environment_absent = "OPENAI_API_KEY" not in child_env
            self.cli_workspace_trusted = project.get("trust_level") == "trusted"
            if (
                auth.get("OPENAI_API_KEY") != "c29-test-only-dummy-key"
                or auth.get("tokens") is not None
                or auth.get("last_refresh") is not None
                or not self.cli_auth_file_store
                or not self.cli_workspace_trusted
                or not self.cli_api_key_environment_absent
            ):
                raise RuntimeError(
                    "CLI auth is not the isolated file-backed dummy credential"
                )
            if "RUST_MIN_STACK" in os.environ:
                child_env["RUST_MIN_STACK"] = os.environ["RUST_MIN_STACK"]
            binary = str(Path(self.config["cli_binary"]).resolve(strict=True))
            if Path(binary).name != "codex":
                raise RuntimeError("paired CLI must be the test-owned codex binary")
            cli_args = [
                binary,
                "--no-daemon",
                "--no-alt-screen",
                "--rencrow-input-author",
                "automation",
            ]
            model = None
            settings = {}
            for index, argument in enumerate(argv):
                if argument == "-m" and index + 1 < len(argv):
                    model = argv[index + 1]
                if argument == "-c" and index + 1 < len(argv):
                    setting = argv[index + 1]
                    key, separator, _ = setting.partition("=")
                    if separator and key in {
                        "model_reasoning_effort",
                        "rencrow_execution_guard.task_kind",
                        "rencrow_parent_handoff_dir",
                        "rencrow_parent_handoff_file",
                    }:
                        settings[key] = self.launcher_toml_value(setting)
            if model != self.config["target_model"]:
                raise RuntimeError(
                    "launcher model differs from sealed capsule settings"
                )
            expected_settings = {
                "model_reasoning_effort": self.config["target_effort"],
                "rencrow_execution_guard.task_kind": "orchestrate",
                "rencrow_parent_handoff_dir": self.config["handoff_directory"],
                "rencrow_parent_handoff_file": self.config["record_file"],
            }
            if settings != expected_settings:
                raise RuntimeError(
                    "launcher config overrides differ from the sealed CLI settings"
                )
            settings_args = []
            for key, value in expected_settings.items():
                settings_args.extend(["-c", key + "=" + json.dumps(value)])
            cli_args.extend(["-m", model, *settings_args])
            master, slave = pty.openpty()
            winsize = struct.pack("HHHH", 40, 120, 0, 0)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, winsize)
            try:
                self.child, self.successor_cli_process_group_id = (
                    start_test_group_process(
                        self.test_process_group_id,
                        cli_args,
                        cwd=self.config["cwd"],
                        env=child_env,
                        stdin=slave,
                        stdout=slave,
                        stderr=slave,
                        close_fds=True,
                    )
                )
            except Exception:
                os.close(slave)
                os.close(master)
                raise
            os.close(slave)
            self.child_master = master
            os.set_blocking(master, False)
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                self.drain_cli_output(0.05)
                if self.child.poll() is not None:
                    raise RuntimeError(
                        f"CLI exited during startup with {self.child.returncode}"
                    )
                try:
                    rollout = self.successor_rollout()
                    if Path(rollout).stat().st_size > 0:
                        break
                except RuntimeError:
                    pass
            else:
                raise RuntimeError("CLI did not create the reserved successor rollout")
            self.task_started_baseline = count_task_started(Path(rollout))
            duplicate_env = dict(
                child_env,
                C29_HANDOFF_PATH=self.config["record_file"],
                C29_WORKSPACE=self.config["cwd"],
                C29_PROVIDER_URL=self.config["provider_url"],
                C29_CODEX_HOME=self.config["home"],
                C29_DUPLICATE_PROOF=str(self.BASE / "duplicate-start-proof.json"),
            )
            duplicate, duplicate_process_group_id = start_test_group_process(
                self.test_process_group_id,
                [
                    self.config["test_binary"],
                    "parent_handoff_os_child_rejects_claimed_record",
                    "--ignored",
                    "--nocapture",
                ],
                cwd=self.config["cwd"],
                env=duplicate_env,
                stdin=subprocess.DEVNULL,
                stdout=self.duplicate_log,
                stderr=subprocess.STDOUT,
            )
            try:
                duplicate_returncode = duplicate.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.stop_owned_process(duplicate)
                raise RuntimeError(
                    "duplicate native start proof timed out and was stopped"
                )
            if duplicate_returncode != 0:
                raise RuntimeError(
                    f"duplicate native start proof failed with {duplicate_returncode}"
                )
            proof = json.loads((self.BASE / "duplicate-start-proof.json").read_text())
            if (
                proof.get("record_file") != self.config["record_file"]
                or proof.get("duplicate_start_child_pid") != duplicate.pid
                or not proof.get("duplicate_start_rejected")
            ):
                raise RuntimeError(
                    "duplicate process did not prove rejection of this exact record"
                )
            self.config["duplicate_native_start_rejected"] = True
            self.config["duplicate_start_child_pid"] = duplicate.pid
            self.config["duplicate_start_returncode"] = duplicate_returncode
            self.config["duplicate_start_process_group_id"] = duplicate_process_group_id
            return
        self.continuation_count += 1
        if self.continuation_count != 1:
            raise RuntimeError("successor continuation was replayed")
        if "Hostの自動引継ぎが完了しました" not in message:
            raise RuntimeError(
                "continuation text does not match the reserved parent handoff"
            )
        if self.child is None or self.child.poll() is not None:
            raise RuntimeError(
                "CLI exited before the single continuation was submitted"
            )
        self.drain_cli_output(0.0)
        os.write(self.child_master, b"\x15")
        os.write(
            self.child_master, b"\x1b[200~" + message.encode("utf-8") + b"\x1b[201~"
        )
        self.continuation_paste_pending = True
        self.input_delivery_sequence.append("bracketed-paste")

    def submit_cli_continuation(self):
        if (
            not self.continuation_paste_pending
            or self.child is None
            or self.child.poll() is not None
            or self.child_master is None
        ):
            raise RuntimeError(
                "CLI continuation is not pending or the owned process is unavailable"
            )
        if self.continuation_submit_count != 0:
            raise RuntimeError("CLI continuation Enter would be replayed")
        os.write(self.child_master, b"\r")
        self.continuation_paste_pending = False
        self.continuation_submit_count += 1
        self.input_delivery_sequence.append("enter")

    @staticmethod
    def launcher_toml_value(setting):
        _, separator, encoded = setting.partition("=")
        if not separator:
            raise RuntimeError("launcher -c argument omits its key/value separator")
        try:
            value = tomllib.loads("value = " + encoded)["value"]
        except (tomllib.TOMLDecodeError, KeyError) as error:
            raise RuntimeError("launcher -c value is not valid TOML") from error
        if not isinstance(value, str):
            raise RuntimeError("launcher C29 config override must decode to a string")
        return value

    def drain_cli_output(self, timeout):
        if self.child_master is None:
            return
        readable, _, _ = select.select([self.child_master], [], [], timeout)
        if not readable:
            return
        while True:
            try:
                chunk = os.read(self.child_master, 65536)
            except BlockingIOError:
                break
            except OSError:
                break
            if not chunk:
                break
            self.child_log.write(chunk)
            self.child_log.flush()

    def close(self):
        if self.child is not None and self.child.poll() is None:
            if self.child_master is not None:
                try:
                    os.write(self.child_master, b"\x03")
                except OSError:
                    pass
            try:
                self.child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.stop_owned_process(self.child)
        child_returncode = self.child.returncode if self.child is not None else None
        child_reaped = self.child is None or self.child.poll() is not None
        if self.child_master is not None:
            self.drain_cli_output(0.0)
            os.close(self.child_master)
            self.child_master = None
        self.child_log.close()
        self.duplicate_log.close()
        if self.source is not None:
            self.stop_owned_process(self.source)
        self.source_returncode = (
            self.source.returncode if self.source is not None else None
        )
        source_reaped = self.source is None or self.source.poll() is not None
        return child_returncode, child_reaped, source_reaped


def cold_owner_probe(host, config, base):
    if host.child is None or host.child.poll() is not None:
        raise RuntimeError("test-owned CLI exited before cold owner observation")
    successor_rollout = Path(host.successor_rollout())
    cold_config = dict(
        config,
        child_pid=host.child.pid,
        test_process_group_id=host.test_process_group_id,
        successor_rollout=str(successor_rollout),
        cold_expected_hashes={
            "ledger": hashlib.sha256(
                (base / "escalation-ledger.json").read_bytes()
            ).hexdigest(),
            "settings": hashlib.sha256(
                (base / "settings.json").read_bytes()
            ).hexdigest(),
            "record": hashlib.sha256(
                Path(config["record_file"]).read_bytes()
            ).hexdigest(),
            "rollout": hashlib.sha256(successor_rollout.read_bytes()).hexdigest(),
        },
    )
    cold_config_path = base / "cold-owner-input.json"
    cold_config_path.write_text(json.dumps(cold_config, ensure_ascii=False))
    cold_env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(host.HOME),
        "PYTHONDONTWRITEBYTECODE": "1",
        "RENCROW_TOOLS_ROOT": os.environ["RENCROW_TOOLS_ROOT"],
        "RENCROW_TOOLS_COMMIT": os.environ["RENCROW_TOOLS_COMMIT"],
        "RENCROW_PARENT_HANDOFF_SHA256": os.environ["RENCROW_PARENT_HANDOFF_SHA256"],
    }
    if "RUST_MIN_STACK" in os.environ:
        cold_env["RUST_MIN_STACK"] = os.environ["RUST_MIN_STACK"]
    cold_process, cold_process_group_id = start_test_group_process(
        host.test_process_group_id,
        [sys.executable, __file__, str(cold_config_path), "--cold-owner"],
        cwd=config["cwd"],
        env=cold_env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        _, cold_stderr = cold_process.communicate(timeout=20)
    except subprocess.TimeoutExpired:
        terminate_owned_process(cold_process)
        _, cold_stderr = cold_process.communicate()
        raise RuntimeError(
            "cold owner subprocess timed out and was stopped: "
            + cold_stderr.decode("utf-8", errors="replace")
        )
    if host.child.poll() is not None:
        raise RuntimeError("test-owned CLI exited during cold owner observation")
    if cold_process.returncode != 0:
        raise RuntimeError(
            "cold owner subprocess failed: "
            + cold_stderr.decode("utf-8", errors="replace")
        )
    result = json.loads((base / "cold-owner-result.json").read_text())
    if (
        result.get("status") != "parent_handoff_continuing"
        or result.get("send_count") != 0
        or result.get("command_count") != 0
        or result.get("child_pid") != host.child.pid
        or result.get("owner_pid") == os.getpid()
        or result.get("same_ledger_bytes_after_observation") is not True
        or result.get("same_settings_record_rollout_bytes_after_observation")
        is not True
        or result.get("ledger_identity_sha256_after")
        != cold_config["cold_expected_hashes"]["ledger"]
    ):
        raise RuntimeError(f"cold owner result violated read-only restore: {result}")
    result["owner_process_returncode"] = cold_process.returncode
    result["owner_process_pid"] = cold_process.pid
    result["owner_process_group_id"] = cold_process_group_id
    (base / "cold-owner-result.json").write_text(
        json.dumps(result, ensure_ascii=False, indent=2)
    )
    return result


def main(config_path):
    config = json.loads(Path(config_path).read_text())
    test_process_group_id = int(config["test_process_group_id"])
    if os.getppid() != test_process_group_id or os.getpgrp() != test_process_group_id:
        raise RuntimeError(
            "Python bridge must be a direct child in the verified nextest process group"
        )
    base = Path(config["base"])
    base.mkdir(parents=True, exist_ok=True)
    task = {
        "id": "c29-test-task",
        "thread": config["source_thread"],
        "spec_file": "isolated-fixture",
    }
    settings = {
        "thread": config["source_thread"],
        "codex_home": config["home"],
        "session": "test-owned",
        "target_model": config["target_model"],
        "target_effort": config["target_effort"],
        "target_mode": "test-owned",
        "parent_handoff": {"enabled": True, "directory": config["handoff_directory"]},
        "model_recovery": {
            "thread": config["source_thread"],
            "executor_policy_file": str(base / "executor-policy.json"),
        },
        "monitored_cli_states": [],
        "allow_product_deployment": False,
    }
    (base / "completion-contract.json").write_text(json.dumps({"active_task": task}))
    (base / "settings.json").write_text(json.dumps(settings))
    (base / "executor-policy.json").write_text(
        json.dumps(
            {
                "owner_thread": config["source_thread"],
                "root_task_id": config["root_task_id"],
                "native_subthreads_allowed": False,
                "executors": {},
            }
        )
    )
    actual_record_sha256 = hashlib.sha256(
        Path(config["record_file"]).read_bytes()
    ).hexdigest()
    if actual_record_sha256 != config["record_sha256"]:
        raise RuntimeError(
            "handoff record bytes changed after the Rust owner supplied them"
        )
    host = Host(dict(config, base=str(base), settings=settings))
    status_history = []
    result_history = []
    cold_result = None
    try:
        for _ in range(300):
            settings = host.target_settings()
            result = parent_handoff.run_parent_handoff(host, host.snapshot(), settings)
            status_history.append(result["status"] if result else None)
            result_history.append(
                {
                    "status": result.get("status") if result else None,
                    "reason": result.get("reason") if result else None,
                }
            )
            if result and result.get("status") == "caution":
                diagnostic = result_history[-1]
                diagnostic["successor_rollout_header"] = successor_rollout_header(
                    config["home"], config["successor_thread"]
                )
                (base / "failure-diagnostic.json").write_text(
                    json.dumps(diagnostic, ensure_ascii=False, indent=2)
                )
                raise RuntimeError(json.dumps(diagnostic, ensure_ascii=False))
            if result and result["status"] == "parent_handoff_adopted":
                break
            if (
                result
                and result["status"] == "parent_handoff_continuing"
                and cold_result is None
            ):
                cold_result = cold_owner_probe(host, config, base)
                host.input_delivery_sequence.append("cold-owner-readonly-resume")
                repeated = parent_handoff.run_parent_handoff(
                    host, host.snapshot(), host.target_settings()
                )
                status_history.append(repeated.get("status") if repeated else None)
                result_history.append(
                    {
                        "status": repeated.get("status") if repeated else None,
                        "reason": repeated.get("reason") if repeated else None,
                    }
                )
                if (
                    not repeated
                    or repeated.get("status") != "parent_handoff_continuing"
                ):
                    raise RuntimeError(
                        f"same-process continuing reread failed: {result_history[-1]}"
                    )
                host.input_delivery_sequence.append("same-process-continuing-reobserve")
                host.submit_cli_continuation()
            time.sleep(0.03)
        if host.launch_count != 1 or host.continuation_count != 1:
            raise RuntimeError(
                f"owner did not preserve at-most-once launch/continuation: {result_history}"
            )
        if host.continuation_submit_count != 1 or host.continuation_paste_pending:
            raise RuntimeError(
                "TUI continuation did not complete exactly one paste/Enter submission"
            )
        if host.exit_keys != ["Escape", "C-u", "C-c", "C-c", "C-u"]:
            raise RuntimeError(f"parent handoff key sequence changed: {host.exit_keys}")
        if not status_history or status_history[-1] != "parent_handoff_adopted":
            raise RuntimeError(
                f"owner did not verify actual CLI turn start: {result_history}"
            )
        if status_history.count("parent_handoff_continuing") < 2:
            raise RuntimeError(
                f"owner did not re-observe the continuing ledger in-process: {result_history}"
            )
        if cold_result is None:
            raise RuntimeError(
                "separate cold owner process was not run while the ledger was continuing"
            )
        if host.child is None or host.child.poll() is not None:
            raise RuntimeError(
                "native successor exited before rollout evidence was copied"
            )
        successor_rollout = Path(host.successor_rollout())
        task_started_count = count_task_started(successor_rollout)
        task_started_delta = task_started_count - host.task_started_baseline
        if task_started_delta != 1:
            raise RuntimeError(
                f"expected one actual successor task_started event after import, baseline={host.task_started_baseline}, final={task_started_count}"
            )
        rollout_bytes = successor_rollout.read_bytes()
        rollout_copy = base / "native-successor-rollout.jsonl"
        rollout_copy.write_bytes(rollout_bytes)
        source_after_copy = successor_rollout.read_bytes()
        copied_rollout_bytes = rollout_copy.read_bytes()
        if rollout_bytes != source_after_copy or rollout_bytes != copied_rollout_bytes:
            raise RuntimeError("native successor rollout changed or copy bytes differ")
        result = {
            "record_file": config["record_file"],
            "record_id": config["record_id"],
            "record_sha256": actual_record_sha256,
            "source_thread": config["source_thread"],
            "successor_thread": config["successor_thread"],
            "statuses": status_history,
            "owner_results": result_history,
            "successor_rollout_source": str(successor_rollout),
            "successor_rollout_copy": str(rollout_copy),
            "successor_rollout_copy_bytes_equal": True,
            "successor_rollout_bytes": len(rollout_bytes),
            "successor_rollout_sha256": hashlib.sha256(rollout_bytes).hexdigest(),
            "task_started_count": task_started_count,
            "task_started_baseline": host.task_started_baseline,
            "task_started_delta": task_started_delta,
            "tools_commit": tools_commit,
            "parent_handoff_source_sha256": actual_handoff_sha256,
            "tools_root": config["tools_root"],
            "parent_handoff_source": str(handoff_source),
            "supervise_source": str(supervisor_root / "supervise.py"),
            "supervise_source_sha256": hashlib.sha256(
                (supervisor_root / "supervise.py").read_bytes()
            ).hexdigest(),
            "launch_count": host.launch_count,
            "continuation_count": host.continuation_count,
            "continuation_submit_count": host.continuation_submit_count,
            "input_delivery_sequence": host.input_delivery_sequence,
            "child_pid": host.child.pid if host.child else None,
            "test_process_id": host.test_process_group_id,
            "python_process_id": os.getpid(),
            "test_process_group_id": host.test_process_group_id,
            "python_process_group_id": os.getpgrp(),
            "source_process_group_id": host.source_process_group_id,
            "source_process_pid": host.source.pid if host.source else None,
            "duplicate_start_process_group_id": host.config.get(
                "duplicate_start_process_group_id"
            ),
            "duplicate_start_process_pid": host.config.get("duplicate_start_child_pid"),
            "successor_cli_process_group_id": host.successor_cli_process_group_id,
            "cli_auth_file_store": host.cli_auth_file_store,
            "cli_auth_dummy_key_only": host.cli_auth_dummy_key_only,
            "cli_api_key_environment_absent": host.cli_api_key_environment_absent,
            "cli_workspace_trusted": host.cli_workspace_trusted,
            "child_returncode_before_cleanup": host.child.poll()
            if host.child
            else None,
            "successor_cli_started": host.child is not None,
            "successor_cli_returncode_before_cleanup": host.child.poll()
            if host.child
            else None,
            "cli_process_proof": str(base / "cli-process-proof.json"),
            "cli_argv_observed": host.cli_process_proof is not None,
            "duplicate_native_start_rejected": host.config.get(
                "duplicate_native_start_rejected"
            )
            is True,
            "duplicate_start_child_pid": host.config.get("duplicate_start_child_pid"),
            "duplicate_start_returncode": host.config.get("duplicate_start_returncode"),
            "owner_final_status": status_history[-1],
            "cli_binary_sha256": config["cli_binary_sha256"],
            "cli_binary_manifest_source_clean": config[
                "cli_binary_manifest_source_clean"
            ],
            "cli_source_commit": config["cli_source_commit"],
            "cli_manifest_path": config["cli_manifest_path"],
            "test_source_head": config["test_source_head"],
            "paired_to_test_source_changes": config["paired_to_test_source_changes"],
            "cli_source_commit_matches_test_head": config[
                "cli_source_commit_matches_test_head"
            ],
            "cli_process_log": str(base / "successor-cli-pty.log"),
            "source_exit_keys": host.exit_keys,
            "child_log": str(base / "successor-cli-pty.log"),
            "cold_owner_result": str(base / "cold-owner-result.json"),
            "cold_owner_pid": cold_result["owner_pid"],
            "cold_owner_process_reaped": cold_result["owner_process_returncode"]
            is not None,
            "cold_owner_process_pid": cold_result["owner_process_pid"],
            "cold_owner_process_group_id": cold_result["owner_process_group_id"],
            "cold_owner_status": cold_result["status"],
            "cold_owner_returncode": cold_result["owner_process_returncode"],
            "cold_owner_reason": cold_result["reason"],
            "cold_owner_send_count": cold_result["send_count"],
            "cold_owner_ledger_sha256": cold_result["ledger_identity_sha256_after"],
            "real_supervisor_restart_tested": False,
        }
    finally:
        returncode, child_reaped, source_reaped = host.close()
    result["child_returncode_after_cleanup"] = returncode
    result["successor_cli_returncode_after_cleanup"] = returncode
    result["successor_cli_inherited_test_group"] = (
        host.child is not None
        and host.successor_cli_process_group_id == host.test_process_group_id
    )
    result["successor_cli_reaped"] = child_reaped
    result["duplicate_start_reaped"] = (
        host.config.get("duplicate_start_returncode") is not None
    )
    result["source_process_reaped"] = source_reaped
    result["cleanup_signal_scope"] = "owned-popen-pids-only"
    result["cleanup_signal_targets"] = host.cleanup_signal_targets
    result["process_group_isolated_by_nextest"] = (
        host.test_process_group_id == os.getppid()
        and host.test_process_group_id == os.getpgrp()
    )
    result["source_process_returncode_after_cleanup"] = host.source_returncode
    result["cold_resume_tested"] = False
    if returncode != 0:
        raise RuntimeError(
            f"native child test exited with {returncode}; see {result['child_log']}"
        )
    (base / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


class ColdOwnerHost:
    """Read-only host observation for a new owner process restoring the ledger."""

    def __init__(self, config):
        self.ROOT = Path(config["cwd"])
        self.BASE = Path(config["base"])
        self.send_count = 0
        self.command_count = 0

    def require_host_action_enabled(self):
        if (self.BASE / "PAUSED").exists():
            raise RuntimeError("test-owned host paused during cold owner observation")

    def send(self, message):
        self.send_count += 1
        raise RuntimeError("cold owner adapter forbids sends")

    def command(self, args):
        self.command_count += 1
        raise RuntimeError("cold owner adapter forbids commands")

    def save(self, path, value):
        raise RuntimeError("cold owner adapter forbids persistence writes")


def cold_owner(config_path):
    config = json.loads(Path(config_path).read_text())
    expected_group_id = int(config["test_process_group_id"])
    if os.getpgrp() != expected_group_id:
        raise RuntimeError(
            "cold owner did not inherit the verified nextest process group"
        )
    base = Path(config["base"])
    ledger_path = base / "escalation-ledger.json"
    settings_path = base / "settings.json"
    record_path = Path(config["record_file"])
    rollout_path = Path(config["successor_rollout"])

    def identities():
        return {
            "ledger": hashlib.sha256(ledger_path.read_bytes()).hexdigest(),
            "settings": hashlib.sha256(settings_path.read_bytes()).hexdigest(),
            "record": hashlib.sha256(record_path.read_bytes()).hexdigest(),
            "rollout": hashlib.sha256(rollout_path.read_bytes()).hexdigest(),
        }

    before = identities()
    if before != config["cold_expected_hashes"]:
        raise RuntimeError(
            "cold owner did not receive the original test-owned file identities"
        )
    ledger, _ = parent_handoff.read_json(ledger_path)
    settings, _ = parent_handoff.read_json(settings_path)
    if settings.get("thread") != config["successor_thread"]:
        raise RuntimeError("cold owner settings do not bind the reserved successor")
    entries = ledger.get("parent_handoffs", {}).get("attempts", {}).values()
    matching = [
        entry
        for entry in entries
        if entry.get("successor_thread_id") == config["successor_thread"]
    ]
    if len(matching) != 1 or matching[0].get("phase") != "continuing":
        raise RuntimeError(
            "cold owner did not restore exactly one continuing ledger entry"
        )
    entry = matching[0]
    if (
        Path(entry.get("record_file", "")).resolve() != record_path.resolve()
        or entry.get("capsule_sha256") != config["record_id"]
        or Path(entry.get("successor_rollout", "")).resolve() != rollout_path.resolve()
    ):
        raise RuntimeError(
            "cold owner ledger does not bind the exact record and successor rollout"
        )
    host = ColdOwnerHost(config)
    result = parent_handoff.run_parent_handoff(
        host,
        {
            "thread": settings["thread"],
            "rollout": str(rollout_path),
            "turn_observation": {"active": False},
            "pending_children": [],
        },
        settings,
    )
    after = identities()
    if not result or result.get("status") != "parent_handoff_continuing":
        raise RuntimeError(f"cold owner did not restore continuing: {result!r}")
    if before != after:
        raise RuntimeError(
            "cold owner observation changed ledger/settings/record/rollout bytes"
        )
    if host.send_count != 0 or host.command_count != 0:
        raise RuntimeError("cold owner attempted a duplicate send or command")
    proof = {
        "owner_pid": os.getpid(),
        "owner_process_group_id": os.getpgrp(),
        "child_pid": config["child_pid"],
        "status": result["status"],
        "reason": result.get("reason"),
        "send_count": host.send_count,
        "command_count": host.command_count,
        "launch_count": 0,
        "continuation_count": 0,
        "ledger_identity_sha256_before": before["ledger"],
        "ledger_identity_sha256_after": after["ledger"],
        "settings_sha256": after["settings"],
        "record_sha256": after["record"],
        "rollout_sha256": after["rollout"],
        "same_ledger_bytes_after_observation": True,
        "same_settings_record_rollout_bytes_after_observation": True,
        "old_owner_stopped": False,
        "real_supervisor_restart_tested": False,
        "real_tui_or_model_turn_tested": False,
    }
    (base / "cold-owner-result.json").write_text(
        json.dumps(proof, ensure_ascii=False, indent=2)
    )
    return 0


def successor_rollout_header(codex_home, successor_thread):
    """Return only native lineage identifiers from the first successor rollout row."""
    sessions = Path(codex_home) / "sessions"
    matches = list(sessions.rglob("*" + successor_thread + "*.jsonl"))
    if len(matches) != 1:
        return None
    try:
        with matches[0].open("rb") as stream:
            row = json.loads(stream.readline(65537))
        payload = row.get("payload", {}) if isinstance(row, dict) else {}
    except (OSError, ValueError):
        return None
    if not isinstance(payload, dict):
        return None
    names = ("id", "session_id", "source", "parent_thread_id", "forked_from_id")
    return {name: payload.get(name) for name in names}


def count_task_started(path):
    count = 0
    for line in path.read_text().splitlines():
        row = json.loads(line)
        if (
            row.get("type") == "event_msg"
            and row.get("payload", {}).get("type") == "task_started"
        ):
            count += 1
    return count


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[2] == "--cold-owner":
        raise SystemExit(cold_owner(sys.argv[1]))
    raise SystemExit(main(sys.argv[1]))
