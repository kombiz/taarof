#!/usr/bin/env python3
"""Disposable real VTE pause/resume/EOF and stalled WebSocket output proof.
Only inert fixture bytes are generated. Tokens remain in request headers only.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
from test_pty_descriptor_boundary import eventually
from test_pty_input_responsiveness import WebSocket


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--probe-library', type=Path, required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    args = parser.parse_args()
    if not Path('/.dockerenv').exists() or os.getuid() == 0:
        parser.error('requires disposable non-root container')
    args.evidence.mkdir(parents=True, exist_ok=True)
    children = []
    sockets = []
    with tempfile.TemporaryDirectory(prefix='taarof-output-') as tmp:
        root = Path(tmp)
        env = {k: v for k, v in os.environ.items() if not k.startswith(('TAAROF_', 'INFISICAL_'))}
        for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR'):
            directory = root / key.lower()
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        probe = root / 'probe'
        probe.mkdir()
        env.update(DISPLAY=':97', GSK_RENDERER='cairo', SHELL='/bin/bash', TAAROF_SESSION='output-probe',
                   LD_PRELOAD=str(args.probe_library), TAAROF_TEST_OUTPUT_DIR=str(probe))
        config = Path(env['XDG_CONFIG_HOME'])
        (config / 'taarof').mkdir()
        (config / 'taarof/config.toml').write_text(
            '[http]\nenabled=true\nport=7849\nbind_address="127.0.0.1"\n'
            '[http_control]\nenabled=true\n[history]\nenabled=false\n'
            '[tasks]\nenabled=false\n[dock]\nvisible=false\n')
        (config / 'ghostty').mkdir()
        (config / 'ghostty/config').write_text('command = /bin/bash --noprofile --norc\n')
        with (args.evidence / 'gtk.log').open('w') as log:
            try:
                display = subprocess.Popen(['Xvfb', ':97', '-screen', '0', '1280x800x24'], stdout=log, stderr=log)
                children.append(display)
                eventually(lambda: Path('/tmp/.X11-unix/X97').exists(), 'display')
                app = subprocess.Popen([str(args.binary)], env=env, stdout=log, stderr=log)
                children.append(app)
                def registry():
                    assert app.poll() is None, 'fixture app exited'
                    for file in Path(env['XDG_RUNTIME_DIR']).glob('taarof-current*.json'):
                        record = json.loads(file.read_text())
                        if record['pid'] == app.pid:
                            return record['socket_path']
                control = eventually(registry, 'socket')
                def request(action, **values):
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.settimeout(2)
                        connection.connect(control)
                        connection.sendall(json.dumps(dict(action=action, **values)).encode())
                        connection.shutdown(socket.SHUT_WR)
                        data = b''
                        while chunk := connection.recv(65536):
                            data += chunk
                    result = json.loads(data)
                    assert result.get('ok'), result.get('error', 'control failed')
                    return result.get('data', result)
                def tabs():
                    return [t for w in request('list-tabs')['workspaces'] for t in w['tabs']]
                def text(tab):
                    return json.dumps(request('get-text', tab=str(tab['tab_id']), pane=tab['panes'][0]['pane_id'], scrollback=100))
                def send(tab, command):
                    request('send-keys', tab=str(tab['tab_id']), pane=tab['panes'][0]['pane_id'], keys=command+'\n')
                def state():
                    assert app.poll() is None, f'fixture app exited {app.returncode}'
                    return json.loads((probe / 'state.json').read_text())
                def command(action, index):
                    (probe / 'pending').write_text(f'{action} {index}')
                    (probe / 'pending').rename(probe / 'command')
                    eventually(lambda: not (probe / 'command').exists(), 'probe command')
                    time.sleep(.03)
                def rss():
                    for line in Path(f'/proc/{app.pid}/status').read_text().splitlines():
                        if line.startswith('VmRSS:'):
                            return int(line.split()[1])
                def producer(name, total, exit_after=False):
                    folder = root / name
                    folder.mkdir()
                    script = folder / 'producer.py'
                    script.write_text('import os,time,tty\nfrom pathlib import Path\n'
                        f'root=Path({str(folder)!r})\n(root/"pid").write_text(str(os.getpid()))\ntty.setraw(0)\nos.write(1,b"OUTPUT_READY")\n'
                        'while not (root/"go").exists():time.sleep(.01)\n'
                        f'total={total}\nwritten=0\n'
                        'while written<total:\n data=b"x"*min(65536,total-written)\n'
                        ' while data:\n  n=os.write(1,data);written+=n;data=data[n:]\n'
                        ' (root/"progress.tmp").write_text(str(written));(root/"progress.tmp").replace(root/"progress")\n'
                        'os.write(1,b"\\r\\nFINAL_SENTINEL")\n(root/"done").touch()\n'+
                        ('' if exit_after else 'while True:time.sleep(1)\n'))
                    return folder, script
                initial = eventually(lambda: tabs(), 'first tab')[0]
                request('create-tab', name='Responsive sibling')
                sibling = eventually(lambda: next((t for t in tabs() if t['tab_id'] != initial['tab_id']), None), 'sibling')
                eventually(lambda: (probe / 'state.json').exists() and len(state()['terminals']) >= 2, 'VTE probe')
                request('switch-tab', tab=str(initial['tab_id']))
                folder, script = producer('native', 8*1024*1024, True)
                send(initial, f'exec python3 {script}')
                eventually(lambda: 'OUTPUT_READY' in text(initial), 'quiescent producer')
                eventually(lambda: any(t['ready'] and t['has_pty'] and t['mapped'] for t in state()['terminals']), 'rendered VTE readiness')
                time.sleep(.2)
                command('pause-ready', 0)
                native_index = state()['last_action_index']
                eventually(lambda: state()['terminals'][native_index]['paused'], 'actual VTE pause')
                command('reset', native_index)
                (folder / 'go').touch()
                samples = []
                for _ in range(4):
                    time.sleep(.5)
                    samples.append(dict(produced=int((folder/'progress').read_text()) if (folder/'progress').exists() else 0,
                                        rss_kib=rss(), vte_bytes=state()['terminals'][native_index]['bytes']))
                assert samples[-1]['produced'] == samples[-2]['produced'] < 8*1024*1024, samples
                assert samples[-1]['vte_bytes'] == 0, samples
                before = state()['ticks']
                started = time.monotonic()
                request('query-state')
                query_ms = (time.monotonic()-started)*1000
                request('switch-tab', tab=str(sibling['tab_id']))
                send(sibling, "printf 'SIBLING_%s\\n' OK")
                eventually(lambda: 'SIBLING_OK' in text(sibling), 'responsive sibling')
                time.sleep(.2)
                ticks = state()['ticks'] - before
                assert query_ms < 600 and ticks >= 10
                command('resume', native_index)
                expected = b'x'*(8*1024*1024)+b'\r\nFINAL_SENTINEL'
                deadline = time.monotonic()+60
                while not state()['terminals'][native_index]['eof']:
                    assert time.monotonic()<deadline, 'natural EOF did not reach VTE'
                    time.sleep(.05)
                native = state()['terminals'][native_index]
                assert native['bytes'] == len(expected), native
                assert native['sha256'] == hashlib.sha256(expected).hexdigest(), native
                assert native['sentinel_at_eof'], native
                eventually(lambda: all(t['tab_id'] != initial['tab_id'] for t in tabs()), 'automatic cleanup after final sentinel')
                # A fresh pane proves stale EOF cannot immediately close the next attachment.
                request('create-tab', name='Stalled WebSocket')
                webtab = eventually(lambda: next((t for t in tabs() if t['tab_id'] != sibling['tab_id']), None), 'web pane')
                folder, script = producer('web', 32*1024*1024)
                send(webtab, f'exec python3 {script}')
                eventually(lambda: 'OUTPUT_READY' in text(webtab), 'web producer')
                tokenfile = Path(env['XDG_RUNTIME_DIR']) / f'taarof-http-{app.pid}.token'
                eventually(tokenfile.is_file, 'fixture token')
                route = f"/api/v1/tabs/{webtab['tab_id']}/panes/{webtab['panes'][0]['pane_id']}/pty/ws"
                token = tokenfile.read_text().strip()
                stalled = WebSocket(7849, route, token)
                sockets.append(stalled.sock)
                assert stalled.receive()['kind'] == 'checkpoint'
                stalled.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                (folder/'go').touch()
                web_samples = []
                deadline = time.monotonic()+90
                while not (folder/'done').exists():
                    assert time.monotonic()<deadline, 'web burst did not complete'
                    time.sleep(.25)
                    web_samples.append(dict(produced=int((folder/'progress').read_text()) if (folder/'progress').exists() else 0,
                                            rss_kib=rss(), ticks=state()['ticks']))
                time.sleep(3)
                # The stalled send's deadline must release its admission slot.
                admitted = []
                for _ in range(16):
                    ws = WebSocket(7849, route, token)
                    sockets.append(ws.sock)
                    assert ws.receive()['kind'] == 'checkpoint', 'stalled observer retained admission'
                    admitted.append(ws)
                excess = http.client.HTTPConnection('127.0.0.1', 7849, timeout=2)
                excess.request('GET', route, headers={'Authorization': 'Bearer '+token,
                    'Connection': 'Upgrade', 'Upgrade': 'websocket',
                    'Sec-WebSocket-Version': '13', 'Sec-WebSocket-Key': 'AAAAAAAAAAAAAAAAAAAAAA=='})
                assert excess.getresponse().status == 429, 'existing HTTP connection cap must reject excess'
                excess.close()
                for ws in admitted:
                    ws.sock.close()
                # Explicit close remains prompt with a truly paused VTE producer.
                request('close-tab', tab=str(webtab['tab_id']))
                request('create-tab', name='Explicit close while paused')
                closing = eventually(lambda: next((t for t in tabs() if t['tab_id'] != sibling['tab_id']), None), 'close pane')
                folder, script = producer('close', 32*1024*1024)
                send(closing, f'exec python3 {script}')
                eventually(lambda: 'OUTPUT_READY' in text(closing), 'close producer')
                request('switch-tab', tab=str(closing['tab_id']))
                eventually(lambda: any(t['ready'] and t['has_pty'] and t['mapped'] for t in state()['terminals']), 'close VTE readiness')
                index = len(state()['terminals'])-1
                time.sleep(.2)
                command('pause-ready', index)
                (folder/'go').touch()
                time.sleep(1)
                started = time.monotonic()
                request('close-tab', tab=str(closing['tab_id']))
                close_ms = (time.monotonic()-started)*1000
                assert close_ms < 600
                closing_pid = int((folder/'pid').read_text())
                eventually(lambda: not Path(f'/proc/{closing_pid}').exists(), 'closed child reaped', timeout=2)
                command('resume', state()['last_action_index'])
                request('query-state')
                # A controlled reader EIO closes the presentation through VTE's
                # valid EOF path. The relay then reports its failed conduit and
                # automatic cleanup must not await a second EOF forever.
                request('create-tab', name='Presentation failure')
                failing = eventually(lambda: next((t for t in tabs() if t['tab_id'] != sibling['tab_id']), None), 'failure pane')
                request('switch-tab', tab=str(failing['tab_id']))
                folder, script = producer('failure', 32*1024*1024)
                send(failing, f'exec python3 {script}')
                eventually(lambda: any(t['ready'] and t['has_pty'] and t['mapped'] for t in state()['terminals']), 'failure VTE readiness')
                command('fail-ready', 0)
                failed_index = state()['last_action_index']
                (folder/'go').touch()
                eventually(lambda: all(t['tab_id'] != failing['tab_id'] for t in tabs()), 'failure cleanup', timeout=8)
                assert state()['terminals'][failed_index]['injected_errors'] == 1
                request('query-state')
                result = dict(native=native, native_paused_samples=samples, query_ms=round(query_ms,2),
                              sibling_round_trip=True, gtk_ticks_while_paused=ticks, natural_cleanup_after_sentinel=True,
                              web_stalled_samples=web_samples, web_admission_released_after_send_deadline=True,
                              presentation_error_cleanup=True,
                              web_observer_limit=16, explicit_close_ms=round(close_ms,2),
                              binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                              process_sha256=hashlib.sha256(Path(f'/proc/{app.pid}/exe').read_bytes()).hexdigest(),
                              vte_version=subprocess.check_output(['pkg-config','--modversion','vte-2.91-gtk4'], text=True).strip())
                assert result['binary_sha256']==result['process_sha256']
                (args.evidence/'result.json').write_text(json.dumps(result,indent=2)+'\n')
                print(json.dumps(result,indent=2))
            finally:
                if (probe/'state.json').exists():
                    (args.evidence/'last-state.json').write_text((probe/'state.json').read_text())
                for connection in sockets:
                    connection.close()
                for child in reversed(children):
                    if child.poll() is None:
                        child.send_signal(signal.SIGTERM)
                        try: child.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            child.kill(); child.wait()


if __name__ == '__main__':
    main()
