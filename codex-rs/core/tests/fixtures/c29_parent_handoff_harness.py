"""Test-owned bridge for the C29 Python owner -> native ThreadManager process path."""
import json
import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

tools_root = Path(os.environ['RENCROW_TOOLS_ROOT'])
supervisor_root = tools_root / 'tools/observability/ren2_supervisor'
handoff_source = supervisor_root / 'parent_handoff.py'
expected_handoff_sha256 = os.environ['RENCROW_PARENT_HANDOFF_SHA256']
actual_handoff_sha256 = hashlib.sha256(handoff_source.read_bytes()).hexdigest()
if actual_handoff_sha256 != expected_handoff_sha256:
    raise RuntimeError('archived Tools parent_handoff.py hash differs from the pinned test source')
tools_commit = os.environ['RENCROW_TOOLS_COMMIT']
sys.path.insert(0, str(supervisor_root))

import parent_handoff
import supervise


class Host:
    def __init__(self, config):
        self.config = config
        self.ROOT = Path(config['cwd'])
        self.BASE = Path(config['base'])
        self.HOME = Path(config['home'])
        self.TMUX = 'test-owned-tmux-adapter'
        self.pid = os.getpid()
        self.source = subprocess.Popen(
            [sys.executable, '-c', 'import time; time.sleep(120)'],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            env={'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
                 'HOME': str(self.HOME), 'PYTHONDONTWRITEBYTECODE': '1'})
        self.child = None
        self.child_log = open(self.BASE / 'native-child.log', 'wb')
        self.child_ready = self.BASE / 'child-ready.json'
        self.child_release = self.BASE / 'child-release'
        self.snapshot_count = 0
        self.launch_count = 0
        self.continuation_count = 0
        self.exit_keys = []
        self.settings = config['settings']
        self.snap = self.source_snapshot()

    def source_snapshot(self):
        live = self.source.poll() is None
        snapshot = {
            'thread': self.config['source_thread'], 'rollout': self.config['source_rollout'],
            'rollout_mtime': self.config['rollout_mtime'], 'rollout_error': None,
            'turn_observation': {'turn_id': self.config['source_turn'], 'active': False,
                                 'tool_running': False, 'compacting': False,
                                 'completion_kind': 'task_complete'},
            'pending_children': [], 'child_rollout_errors': [],
            'child_receipts': {'unknown': []},
            'target_processes': ([{'pid': self.source.pid,
                                   'command': f'/test/rencrow-switch-core resume {self.config["source_thread"]} --rencrow-input-author automation'}]
                                 if live else []),
            'processes': ([f'{self.source.pid} {self.pid} 00:01 /test/rencrow-switch-core resume {self.config["source_thread"]} --rencrow-input-author automation']
                          if live else []),
            'process_observation': {'returncode': 0, 'monitored_cli_processes': []},
        }
        return snapshot

    def successor_snapshot(self):
        return {
            'thread': self.config['successor_thread'], 'rollout': self.successor_rollout(),
            'rollout_mtime': 1, 'rollout_error': None,
            'turn_observation': {'turn_id': self.config['source_turn'], 'active': False,
                                 'tool_running': False, 'compacting': False},
            'pending_children': [], 'child_rollout_errors': [],
            'child_receipts': {'unknown': []}, 'target_processes': [], 'processes': [],
            'process_observation': {'returncode': 0, 'monitored_cli_processes': []},
        }

    def successor_rollout(self):
        sessions = Path(self.config['home']) / 'sessions'
        matches = list(sessions.rglob('*' + self.config['successor_thread'] + '*.jsonl'))
        if len(matches) != 1:
            raise RuntimeError(f'expected exactly one native successor rollout, found {len(matches)}')
        return str(matches[0])

    def require_host_action_enabled(self):
        if (self.BASE / 'PAUSED').exists():
            raise RuntimeError('test-owned host paused')

    def validate_writer_observation(self, snap, settings):
        observation = snap.get('process_observation', {'returncode': 0,
                                                        'monitored_cli_processes': []})
        if observation.get('returncode') != 0 or observation.get('monitored_cli_processes') != []:
            raise RuntimeError('test-owned process observation is not quiescent')

    @staticmethod
    def turn_was_user_aborted(turn):
        return turn.get('completion_kind') == 'turn_aborted'

    def active_task(self):
        return json.loads((self.BASE / 'completion-contract.json').read_text())['active_task']

    def target_process_pid(self, snap):
        if snap.get('thread') == self.config['source_thread']:
            return self.source.pid
        if self.child is None:
            raise RuntimeError('native successor process is absent')
        return self.child.pid

    def target_input_author(self, snap, pid):
        return supervise.target_input_author(snap, pid)

    def has_executing_subprocesses(self, pids):
        return False

    def snapshot(self):
        self.snapshot_count += 1
        self.settings = self.target_settings()
        if self.settings['thread'] == self.config['successor_thread']:
            snap = self.successor_snapshot()
            if self.child is None or self.child.poll() is not None:
                raise RuntimeError('native successor process is not alive')
            command = f'{self.config["test_binary"]} parent_handoff_os_child_imports_same_record'
            snap.update({
                'pane': f'%test {self.child.pid} codex-test-child',
                'target_processes': [{'pid': self.child.pid, 'command': command}],
                'processes': [f'{self.child.pid} {self.pid} 00:01 {command}'],
                'process_tree': [{'pid': self.child.pid, 'ppid': self.pid}],
            })
            return snap
        return self.source_snapshot()

    def target_settings(self):
        self.settings = json.loads((self.BASE / 'settings.json').read_text())
        return self.settings

    def save(self, path, value):
        Path(path).write_text(json.dumps(value, ensure_ascii=False))

    def command(self, args):
        if args[1:2] == ['display-message']:
            return subprocess.CompletedProcess(args, 0, 'zsh\n', '')
        if args[1:2] == ['send-keys']:
            key = args[-1]
            self.exit_keys.append(key)
            if key == 'C-c' and self.exit_keys.count('C-c') == 2:
                self.source.terminate()
                self.source.wait(timeout=5)
            if key == 'C-u':
                active_thread = self.target_settings()['thread']
                if active_thread == self.config['source_thread']:
                    if self.exit_keys != ['Escape', 'C-u']:
                        raise RuntimeError('source exit sequence is out of order')
                elif (active_thread == self.config['successor_thread']
                      and self.exit_keys.count('C-u') != 2):
                    raise RuntimeError('successor input clear is out of order')
                elif active_thread not in [self.config['source_thread'], self.config['successor_thread']]:
                    raise RuntimeError('input clear targets an unknown owner')
            return subprocess.CompletedProcess(args, 0, '', '')
        raise RuntimeError('unexpected test-owned command: ' + repr(args))

    def send(self, message):
        argv = shlex.split(message)
        handoff_values = []
        for index, argument in enumerate(argv):
            if argument == '-c' and index + 1 < len(argv):
                setting = argv[index + 1]
                if setting.startswith('rencrow_parent_handoff_file='):
                    import tomllib
                    handoff_values.append(tomllib.loads('value = ' + setting.split('=', 1)[1])['value'])
        is_new_parent = '--new' in argv or any(
            argument.startswith('rencrow_parent_handoff_file=') for argument in argv)
        if self.launch_count == 0 and self.continuation_count == 0 and not is_new_parent:
            raise RuntimeError('first owner send did not contain a native parent launcher request')
        if is_new_parent:
            if '--new' not in argv or len(handoff_values) != 1:
                raise RuntimeError('launcher handoff options are missing or ambiguous')
            requested_record = str(Path(handoff_values[0]).resolve(strict=True))
            expected_record = str(Path(self.config['record_file']).resolve(strict=True))
            if requested_record != expected_record:
                raise RuntimeError('launcher did not pass the exact verified handoff record')
            if '--rencrow-input-author' not in argv or 'automation' not in argv:
                raise RuntimeError('launcher input author is not the required automation identity')
            self.launch_count += 1
            if self.launch_count != 1:
                raise RuntimeError('native successor launch was replayed')
            child_env = {
                'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
                'HOME': str(self.HOME),
                'C29_HANDOFF_PATH': self.config['record_file'],
                'C29_WORKSPACE': self.config['cwd'],
                'C29_PROVIDER_URL': self.config['provider_url'],
                'C29_CODEX_HOME': self.config['home'],
                'C29_READY_FILE': str(self.child_ready),
                'C29_RELEASE_FILE': str(self.child_release),
                'C29_DONE_FILE': str(self.BASE / 'child-done.json'),
                'PYTHONDONTWRITEBYTECODE': '1',
            }
            if 'RUST_MIN_STACK' in os.environ:
                child_env['RUST_MIN_STACK'] = os.environ['RUST_MIN_STACK']
            self.child = subprocess.Popen(
                [self.config['test_binary'], 'parent_handoff_os_child_imports_same_record',
                 '--ignored', '--nocapture'],
                cwd=self.config['cwd'], env=child_env, stdin=subprocess.DEVNULL,
                stdout=self.child_log, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and not self.child_ready.exists():
                if self.child.poll() is not None:
                    raise RuntimeError('native child exited before proving record import')
                time.sleep(0.02)
            if not self.child_ready.exists():
                raise RuntimeError('native child did not publish its import proof')
            proof = json.loads(self.child_ready.read_text())
            if proof.get('record_file') != self.config['record_file'] or proof.get('record_id') != self.config['record_id']:
                raise RuntimeError('native child imported a different handoff record')
            return
        self.continuation_count += 1
        if self.continuation_count != 1:
            raise RuntimeError('successor continuation was replayed')

    def close(self):
        if self.child is not None and self.child.poll() is None:
            self.child_release.touch()
            try:
                self.child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.child.terminate()
                try:
                    self.child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.child.kill()
                    self.child.wait(timeout=5)
        child_returncode = self.child.returncode if self.child is not None else None
        self.child_log.close()
        if self.source.poll() is None:
            self.source.terminate()
            try:
                self.source.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.source.kill()
                self.source.wait(timeout=5)
        return child_returncode


def main(config_path):
    config = json.loads(Path(config_path).read_text())
    base = Path(config['base'])
    base.mkdir(parents=True, exist_ok=True)
    task = {'id': 'c29-test-task', 'thread': config['source_thread'], 'spec_file': 'isolated-fixture'}
    settings = {
        'thread': config['source_thread'], 'codex_home': config['home'], 'session': 'test-owned',
        'target_model': 'test-parent', 'target_effort': 'high', 'target_mode': 'test-owned',
        'parent_handoff': {'enabled': True, 'directory': config['handoff_directory']},
        'model_recovery': {'thread': config['source_thread'],
                           'executor_policy_file': str(base / 'executor-policy.json')},
        'monitored_cli_states': [], 'allow_product_deployment': False,
    }
    (base / 'completion-contract.json').write_text(json.dumps({'active_task': task}))
    (base / 'settings.json').write_text(json.dumps(settings))
    (base / 'executor-policy.json').write_text(json.dumps({
        'owner_thread': config['source_thread'], 'root_task_id': config['root_task_id'],
        'native_subthreads_allowed': False, 'executors': {},
    }))
    actual_record_sha256 = hashlib.sha256(Path(config['record_file']).read_bytes()).hexdigest()
    if actual_record_sha256 != config['record_sha256']:
        raise RuntimeError('handoff record bytes changed after the Rust owner supplied them')
    host = Host(dict(config, base=str(base), settings=settings))
    status_history = []
    result_history = []
    try:
        for _ in range(20):
            settings = host.target_settings()
            result = parent_handoff.run_parent_handoff(host, host.snapshot(), settings)
            status_history.append(result['status'] if result else None)
            result_history.append({
                'status': result.get('status') if result else None,
                'reason': result.get('reason') if result else None,
            })
            if result and result.get('status') == 'caution':
                diagnostic = result_history[-1]
                diagnostic['successor_rollout_header'] = successor_rollout_header(
                    config['home'], config['successor_thread'])
                (base / 'failure-diagnostic.json').write_text(
                    json.dumps(diagnostic, ensure_ascii=False, indent=2))
                raise RuntimeError(json.dumps(diagnostic, ensure_ascii=False))
            if result and result['status'] == 'parent_handoff_continuing':
                # Re-observe the same native import: continuation must remain one-shot.
                repeated = parent_handoff.run_parent_handoff(
                    host, host.snapshot(), host.target_settings())
                status_history.append(repeated['status'] if repeated else None)
                result_history.append({
                    'status': repeated.get('status') if repeated else None,
                    'reason': repeated.get('reason') if repeated else None,
                })
                if repeated and repeated.get('status') == 'caution':
                    diagnostic = result_history[-1]
                    diagnostic['successor_rollout_header'] = successor_rollout_header(
                        config['home'], config['successor_thread'])
                    (base / 'failure-diagnostic.json').write_text(
                        json.dumps(diagnostic, ensure_ascii=False, indent=2))
                    raise RuntimeError(json.dumps(diagnostic, ensure_ascii=False))
                if repeated and repeated['status'] == 'parent_handoff_continuing':
                    break
            time.sleep(0.03)
        if host.launch_count != 1 or host.continuation_count != 1:
            raise RuntimeError(
                f'owner did not preserve at-most-once launch/continuation: {result_history}')
        if status_history.count('parent_handoff_continuing') < 2:
            raise RuntimeError(
                f'owner did not re-observe the native import in the same process: {result_history}')
        if host.child is None or host.child.poll() is not None:
            raise RuntimeError('native successor exited before rollout evidence was copied')
        successor_rollout = Path(host.successor_rollout())
        cold_config = dict(config, child_pid=host.child.pid,
                           successor_rollout=str(successor_rollout),
                           cold_expected_hashes={
                               'ledger': hashlib.sha256(
                                   (base / 'escalation-ledger.json').read_bytes()).hexdigest(),
                               'settings': hashlib.sha256(
                                   (base / 'settings.json').read_bytes()).hexdigest(),
                               'record': hashlib.sha256(
                                   Path(config['record_file']).read_bytes()).hexdigest(),
                               'rollout': hashlib.sha256(
                                   successor_rollout.read_bytes()).hexdigest(),
                           })
        cold_config_path = base / 'cold-owner-input.json'
        cold_config_path.write_text(json.dumps(cold_config, ensure_ascii=False))
        cold_env = {
            'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
            'HOME': str(host.HOME),
            'PYTHONDONTWRITEBYTECODE': '1',
            'RENCROW_TOOLS_ROOT': os.environ['RENCROW_TOOLS_ROOT'],
            'RENCROW_TOOLS_COMMIT': os.environ['RENCROW_TOOLS_COMMIT'],
            'RENCROW_PARENT_HANDOFF_SHA256': os.environ['RENCROW_PARENT_HANDOFF_SHA256'],
        }
        cold_process = subprocess.run(
            [sys.executable, __file__, str(cold_config_path), '--cold-owner'],
            cwd=config['cwd'], env=cold_env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20, check=False)
        if host.child.poll() is not None:
            raise RuntimeError('native successor exited during cold owner observation')
        if cold_process.returncode != 0:
            raise RuntimeError(
                'cold owner subprocess failed: '
                + cold_process.stderr.decode('utf-8', errors='replace'))
        cold_result = json.loads((base / 'cold-owner-result.json').read_text())
        if (cold_result.get('status') != 'parent_handoff_continuing'
                or cold_result.get('send_count') != 0
                or cold_result.get('command_count') != 0
                or cold_result.get('child_pid') != host.child.pid
                or cold_result.get('owner_pid') == os.getpid()
                or cold_result.get('same_ledger_bytes_after_observation') is not True
                or cold_result.get('same_settings_record_rollout_bytes_after_observation') is not True
                or cold_result.get('ledger_identity_sha256_after')
                != cold_config['cold_expected_hashes']['ledger']):
            raise RuntimeError(f'cold owner result violated read-only restore: {cold_result}')
        rollout_bytes = successor_rollout.read_bytes()
        rollout_copy = base / 'native-successor-rollout.jsonl'
        rollout_copy.write_bytes(rollout_bytes)
        source_after_copy = successor_rollout.read_bytes()
        copied_rollout_bytes = rollout_copy.read_bytes()
        if rollout_bytes != source_after_copy or rollout_bytes != copied_rollout_bytes:
            raise RuntimeError('native successor rollout changed or copy bytes differ')
        result = {
            'record_file': config['record_file'], 'record_id': config['record_id'],
            'record_sha256': actual_record_sha256, 'source_thread': config['source_thread'],
            'successor_thread': config['successor_thread'], 'statuses': status_history,
            'owner_results': result_history,
            'successor_rollout_source': str(successor_rollout),
            'successor_rollout_copy': str(rollout_copy),
            'successor_rollout_copy_bytes_equal': True,
            'successor_rollout_bytes': len(rollout_bytes),
            'successor_rollout_sha256': hashlib.sha256(rollout_bytes).hexdigest(),
            'tools_commit': tools_commit, 'parent_handoff_source_sha256': actual_handoff_sha256,
            'launch_count': host.launch_count, 'continuation_count': host.continuation_count,
            'child_pid': host.child.pid if host.child else None,
            'child_returncode_before_cleanup': host.child.poll() if host.child else None,
            'source_exit_keys': host.exit_keys,
            'child_log': str(base / 'native-child.log'),
            'cold_owner_result': str(base / 'cold-owner-result.json'),
            'cold_owner_pid': cold_result['owner_pid'],
            'cold_owner_status': cold_result['status'],
            'cold_owner_reason': cold_result['reason'],
            'cold_owner_send_count': cold_result['send_count'],
            'cold_owner_ledger_sha256': cold_result['ledger_identity_sha256_after'],
            'real_supervisor_restart_tested': False,
        }
    finally:
        returncode = host.close()
    result['child_returncode_after_cleanup'] = returncode
    result['cold_resume_tested'] = False
    if returncode != 0:
        raise RuntimeError(f'native child test exited with {returncode}; see {result["child_log"]}')
    (base / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


class ColdOwnerHost:
    """Read-only host observation for a new owner process restoring the ledger."""
    def __init__(self, config):
        self.ROOT = Path(config['cwd'])
        self.BASE = Path(config['base'])
        self.send_count = 0
        self.command_count = 0

    def require_host_action_enabled(self):
        if (self.BASE / 'PAUSED').exists():
            raise RuntimeError('test-owned host paused during cold owner observation')

    def send(self, message):
        self.send_count += 1
        raise RuntimeError('cold owner adapter forbids sends')

    def command(self, args):
        self.command_count += 1
        raise RuntimeError('cold owner adapter forbids commands')

    def save(self, path, value):
        raise RuntimeError('cold owner adapter forbids persistence writes')


def cold_owner(config_path):
    config = json.loads(Path(config_path).read_text())
    base = Path(config['base'])
    ledger_path = base / 'escalation-ledger.json'
    settings_path = base / 'settings.json'
    record_path = Path(config['record_file'])
    rollout_path = Path(config['successor_rollout'])

    def identities():
        return {
            'ledger': hashlib.sha256(ledger_path.read_bytes()).hexdigest(),
            'settings': hashlib.sha256(settings_path.read_bytes()).hexdigest(),
            'record': hashlib.sha256(record_path.read_bytes()).hexdigest(),
            'rollout': hashlib.sha256(rollout_path.read_bytes()).hexdigest(),
        }

    before = identities()
    if before != config['cold_expected_hashes']:
        raise RuntimeError('cold owner did not receive the original test-owned file identities')
    ledger, _ = parent_handoff.read_json(ledger_path)
    settings, _ = parent_handoff.read_json(settings_path)
    if settings.get('thread') != config['successor_thread']:
        raise RuntimeError('cold owner settings do not bind the reserved successor')
    entries = ledger.get('parent_handoffs', {}).get('attempts', {}).values()
    matching = [entry for entry in entries
                if entry.get('successor_thread_id') == config['successor_thread']]
    if len(matching) != 1 or matching[0].get('phase') != 'continuing':
        raise RuntimeError('cold owner did not restore exactly one continuing ledger entry')
    entry = matching[0]
    if (Path(entry.get('record_file', '')).resolve() != record_path.resolve()
            or entry.get('capsule_sha256') != config['record_id']
            or Path(entry.get('successor_rollout', '')).resolve() != rollout_path.resolve()):
        raise RuntimeError('cold owner ledger does not bind the exact record and successor rollout')
    host = ColdOwnerHost(config)
    result = parent_handoff.run_parent_handoff(host, {
        'thread': settings['thread'], 'rollout': str(rollout_path),
        'turn_observation': {'active': False}, 'pending_children': [],
    }, settings)
    after = identities()
    if not result or result.get('status') != 'parent_handoff_continuing':
        raise RuntimeError(f'cold owner did not restore continuing: {result!r}')
    if before != after:
        raise RuntimeError('cold owner observation changed ledger/settings/record/rollout bytes')
    if host.send_count != 0 or host.command_count != 0:
        raise RuntimeError('cold owner attempted a duplicate send or command')
    proof = {
        'owner_pid': os.getpid(), 'child_pid': config['child_pid'],
        'status': result['status'], 'reason': result.get('reason'),
        'send_count': host.send_count, 'command_count': host.command_count,
        'launch_count': 0, 'continuation_count': 0,
        'ledger_identity_sha256_before': before['ledger'],
        'ledger_identity_sha256_after': after['ledger'],
        'settings_sha256': after['settings'], 'record_sha256': after['record'],
        'rollout_sha256': after['rollout'],
        'same_ledger_bytes_after_observation': True,
        'same_settings_record_rollout_bytes_after_observation': True,
        'old_owner_stopped': False, 'real_supervisor_restart_tested': False,
        'real_tui_or_model_turn_tested': False,
    }
    (base / 'cold-owner-result.json').write_text(
        json.dumps(proof, ensure_ascii=False, indent=2))
    return 0


def successor_rollout_header(codex_home, successor_thread):
    """Return only native lineage identifiers from the first successor rollout row."""
    sessions = Path(codex_home) / 'sessions'
    matches = list(sessions.rglob('*' + successor_thread + '*.jsonl'))
    if len(matches) != 1:
        return None
    try:
        with matches[0].open('rb') as stream:
            row = json.loads(stream.readline(65537))
        payload = row.get('payload', {}) if isinstance(row, dict) else {}
    except (OSError, ValueError):
        return None
    if not isinstance(payload, dict):
        return None
    names = ('id', 'session_id', 'source', 'parent_thread_id', 'forked_from_id')
    return {name: payload.get(name) for name in names}


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[2] == '--cold-owner':
        raise SystemExit(cold_owner(sys.argv[1]))
    raise SystemExit(main(sys.argv[1]))
