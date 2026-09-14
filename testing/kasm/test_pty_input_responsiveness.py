#!/usr/bin/env python3
"""Unread real PTY input versus GTK heartbeat, sibling keyboard and cancellation.

Requires a disposable, non-root Kasm container, no published ports, and the
compiled gtk_input_timer_probe.c library. Only this fixture's generated bearer
is read, exclusively into an Authorization header; it is never printed or saved.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time

from test_pty_descriptor_boundary import eventually


class WebSocket:
    def __init__(self, port, route, token):
        self.sock = socket.create_connection(('127.0.0.1', port), timeout=5)
        self.buffer = b''
        key = base64.b64encode(os.urandom(16)).decode()
        request = (f'GET {route} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n'
                   'Upgrade: websocket\r\nConnection: Upgrade\r\n'
                   f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n'
                   f'Authorization: Bearer {token}\r\n\r\n')
        self.sock.sendall(request.encode())
        while b'\r\n\r\n' not in self.buffer:
            self.buffer += self.sock.recv(4096)
        header, self.buffer = self.buffer.split(b'\r\n\r\n', 1)
        assert header.split(b'\r\n')[0].split()[1] == b'101', 'upgrade failed'

    def read(self, count):
        while len(self.buffer) < count:
            chunk = self.sock.recv(max(count - len(self.buffer), 4096))
            assert chunk, 'websocket closed'
            self.buffer += chunk
        result, self.buffer = self.buffer[:count], self.buffer[count:]
        return result

    def send(self, data, opcode=1):
        payload = json.dumps(data).encode() if opcode == 1 else data
        length = len(payload)
        header = bytes([0x80 | opcode, 0x80 | (length if length < 126 else 126 if length < 65536 else 127)])
        if length >= 126:
            header += struct.pack('!H' if length < 65536 else '!Q', length)
        mask = os.urandom(4)
        self.sock.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def receive(self):
        while True:
            opcode, length = self.read(2)
            assert not length & 128
            length &= 127
            if length >= 126:
                length = struct.unpack('!H' if length == 126 else '!Q', self.read(2 if length == 126 else 8))[0]
            payload = self.read(length)
            if opcode & 15 == 9:
                self.send(payload, 10)
            elif opcode & 15 == 1:
                return json.loads(payload)
            else:
                assert opcode & 15 != 8, 'server closed websocket'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--timer-library', type=Path, required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--expect-stall', action='store_true')
    args = parser.parse_args()
    if not Path('/.dockerenv').is_file() or os.getuid() == 0:
        parser.error('requires disposable non-root container')
    args.evidence.mkdir(parents=True, exist_ok=True)
    children = []
    with tempfile.TemporaryDirectory(prefix='taarof-input-') as tmp:
        root = Path(tmp)
        env = {k: v for k, v in os.environ.items() if not k.startswith(('TAAROF_', 'INFISICAL_'))}
        for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR'):
            directory = root / key.lower()
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        tick_file = root / 'ticks'
        env.update(DISPLAY=':97', GSK_RENDERER='cairo', SHELL='/bin/bash',
                   TAAROF_SESSION='input-probe', LD_PRELOAD=str(args.timer_library),
                   TAAROF_TEST_TICK_FILE=str(tick_file))
        config = Path(env['XDG_CONFIG_HOME'])
        (config / 'taarof').mkdir()
        (config / 'taarof/config.toml').write_text(
            '[http]\nenabled = true\nport = 7849\nbind_address = "127.0.0.1"\n'
            '[http_control]\nenabled = true\n[history]\nenabled = false\n'
            '[tasks]\nenabled = false\n[dock]\nvisible = false\n')
        (config / 'ghostty').mkdir()
        (config / 'ghostty/config').write_text('command = /bin/bash --noprofile --norc\n')
        child = root / 'unread.py'
        child.write_text('import os,tty,time,select\nfrom pathlib import Path\n'
                         f'root=Path({str(root)!r})\ntty.setraw(0)\nprint("UNREAD_READY",flush=True)\n'
                         'while not (root/"go").exists(): time.sleep(.01)\n'
                         'data=b""\nwhile select.select([0],[],[],.4)[0]:\n data+=os.read(0,65536)\n'
                         '(root/"received").write_bytes(data)\nwhile True: time.sleep(1)\n')
        with (args.evidence / 'gtk.log').open('w') as log:
            try:
                display = subprocess.Popen(['Xvfb', ':97', '-screen', '0', '1280x800x24'], stdout=log, stderr=log)
                children.append(display)
                eventually(lambda: Path('/tmp/.X11-unix/X97').exists(), 'display')
                app = subprocess.Popen([str(args.binary)], env=env, stdout=log, stderr=log)
                children.append(app)

                def registry():
                    assert app.poll() is None, 'app exited'
                    for record in Path(env['XDG_RUNTIME_DIR']).glob('taarof-current*.json'):
                        data = json.loads(record.read_text())
                        if data['pid'] == app.pid:
                            return data['socket_path']
                sock = eventually(registry, 'socket')

                def request(action, timeout=2, **kw):
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.settimeout(timeout)
                        connection.connect(sock)
                        connection.sendall(json.dumps(dict(action=action, **kw)).encode())
                        connection.shutdown(socket.SHUT_WR)
                        data = b''
                        while chunk := connection.recv(65536):
                            data += chunk
                    response = json.loads(data)
                    assert response.get('ok'), response.get('error', 'request failed')
                    return response.get('data', response)

                def tabs():
                    return [t for w in request('list-tabs')['workspaces'] for t in w['tabs']]

                def text(tab, pane):
                    return json.dumps(request('get-text', tab=str(tab), pane=pane, scrollback=100))

                def ticks():
                    return struct.unpack('=Q', tick_file.read_bytes())[0]

                initial = eventually(lambda: tabs(), 'first tab')[0]
                blocked_tab, blocked_pane = initial['tab_id'], initial['panes'][0]['pane_id']
                request('send-keys', tab=str(blocked_tab), pane=blocked_pane, keys=f'python3 {child}\n')
                eventually(lambda: 'UNREAD_READY' in text(blocked_tab, blocked_pane), 'unread child')
                request('create-tab', name='Responsive sibling')
                sibling = eventually(lambda: next((t for t in tabs() if t['tab_id'] != blocked_tab), None), 'sibling')
                sibling_tab, sibling_pane = sibling['tab_id'], sibling['panes'][0]['pane_id']
                request('switch-tab', tab=str(sibling_tab))
                time.sleep(.2)
                token_path = Path(env['XDG_RUNTIME_DIR']) / f'taarof-http-{app.pid}.token'
                eventually(token_path.is_file, 'fixture HTTP token')
                websocket = WebSocket(7849, f'/api/v1/tabs/{blocked_tab}/panes/{blocked_pane}/pty/ws', token_path.read_text().strip())
                checkpoint = websocket.receive()
                assert checkpoint['kind'] == 'checkpoint'
                frame = {key: checkpoint[key] for key in ('protocol_version', 'runtime_id', 'session_name', 'tab_id', 'pane_id', 'epoch')}
                frame.update(kind='input', input_seq='1', grant_generation='1',
                             nonce='00000000-0000-4000-8000-000000000002',
                             deadline_ms=int(time.time() * 1000) + 2000,
                             payload_base64=base64.b64encode(b'x' * 65536).decode(), byte_count=65536)
                eventually(lambda: tick_file.exists() and tick_file.stat().st_size == 8, 'GLib heartbeat')
                websocket.send(frame)
                time.sleep(.15)
                before = ticks()
                start = time.monotonic()
                query_ok = True
                try:
                    request('query-state', timeout=.6)
                except TimeoutError:
                    query_ok = False
                query_ms = (time.monotonic() - start) * 1000
                windows = subprocess.check_output(['xdotool', 'search', '--onlyvisible', '--pid', str(app.pid)], env=env).splitlines()
                subprocess.run(['xdotool', 'windowfocus', '--sync', windows[-1].decode()], env=env, check=True)
                subprocess.run(['xdotool', 'mousemove', '--window', windows[-1].decode(), '450', '200', 'click', '1'], env=env, check=True)
                subprocess.run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', "printf 'SIBLING_%s\\n' OK"], env=env, check=True)
                subprocess.run(['xdotool', 'key', 'Return'], env=env, check=True)
                time.sleep(.3)
                advance = ticks() - before
                sibling_ok = False
                if query_ok:
                    sibling_text = text(sibling_tab, sibling_pane)
                    (args.evidence / 'sibling-text.json').write_text(sibling_text)
                    sibling_ok = 'SIBLING_OK' in sibling_text
                from PIL import ImageGrab
                ImageGrab.grab(xdisplay=env['DISPLAY']).save(args.evidence / 'gtk.png')
                result = dict(expect_stall=args.expect_stall, query_completed=query_ok,
                              query_ms=round(query_ms, 2), glib_ticks_while_unread=advance,
                              sibling_keyboard_round_trip=sibling_ok,
                              binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                              process_sha256=hashlib.sha256(Path(f'/proc/{app.pid}/exe').read_bytes()).hexdigest())
                if args.expect_stall:
                    assert not query_ok and advance == 0 and not sibling_ok, result
                    (root / 'go').touch()
                else:
                    assert query_ok and query_ms < 600 and advance >= 10 and sibling_ok, result
                    while True:
                        response = websocket.receive()
                        if 'input_result' in response:
                            break
                    outcome = response['input_result']
                    assert outcome['status'] == 'deadline_expired', outcome
                    assert 0 < outcome['written_bytes'] < 65536, outcome
                    assert outcome['requested_bytes'] == 65536 and outcome['input_seq'] == '1', outcome
                    (root / 'go').touch()
                    eventually(lambda: (root / 'received').exists(), 'drained accepted prefix')
                    received = (root / 'received').read_bytes()
                    assert received == b'x' * outcome['written_bytes'], 'cancelled remainder delivered'
                    result.update(input_outcome=outcome, received_bytes=len(received), cancelled_remainder_absent=True)
                    close_start = time.monotonic()
                    request('close-tab', tab=str(blocked_tab))
                    result['close_tab_ms'] = round((time.monotonic() - close_start) * 1000, 2)
                    assert result['close_tab_ms'] < 600
                    # A second unread child verifies actual transport disconnect,
                    # independently of the deadline path and receipt unit tests.
                    disconnected_root = root / 'disconnect'
                    disconnected_root.mkdir()
                    disconnected_child = root / 'disconnect.py'
                    disconnected_child.write_text(child.read_text().replace(repr(str(root)), repr(str(disconnected_root))))
                    request('create-tab', name='Disconnect cancellation')
                    disconnected_tab = eventually(lambda: next((t for t in tabs() if t['tab_id'] != sibling_tab), None), 'disconnect tab')
                    dt, dp = disconnected_tab['tab_id'], disconnected_tab['panes'][0]['pane_id']
                    request('send-keys', tab=str(dt), pane=dp, keys=f'python3 {disconnected_child}\n')
                    eventually(lambda: 'UNREAD_READY' in text(dt, dp), 'disconnect unread child')
                    disconnected = WebSocket(7849, f'/api/v1/tabs/{dt}/panes/{dp}/pty/ws', token_path.read_text().strip())
                    cp = disconnected.receive()
                    df = {key: cp[key] for key in ('protocol_version', 'runtime_id', 'session_name', 'tab_id', 'pane_id', 'epoch')}
                    df.update({key: value for key, value in frame.items() if key not in df})
                    df['deadline_ms'] = int(time.time() * 1000) + 5000
                    disconnected.send(df)
                    time.sleep(.15)
                    disconnected.sock.close()
                    time.sleep(.2)
                    (disconnected_root / 'go').touch()
                    eventually(lambda: (disconnected_root / 'received').exists(), 'disconnect prefix')
                    disconnected_bytes = (disconnected_root / 'received').read_bytes()
                    assert 0 < len(disconnected_bytes) < 65536 and disconnected_bytes == b'x' * len(disconnected_bytes)
                    result.update(disconnect_received_bytes=len(disconnected_bytes), disconnect_remainder_absent=True)
                    request('close-tab', tab=str(dt))
                    from PIL import ImageGrab
                    ImageGrab.grab(xdisplay=env['DISPLAY']).save(args.evidence / 'gtk.png')
                websocket.sock.close()
                assert result['binary_sha256'] == result['process_sha256']
                (args.evidence / 'gtk-result.json').write_text(json.dumps(result, indent=2) + '\n')
                print(json.dumps(result, indent=2))
            finally:
                (root / 'go').touch()
                for child in reversed(children):
                    if child.poll() is None:
                        child.send_signal(signal.SIGTERM)
                        try:
                            child.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            child.kill()
                            child.wait()


if __name__ == '__main__':
    main()
