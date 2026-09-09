"""Bounded extension to test-network-access.py: no additional guests.

At most 16 connections, two slow readers, and three gateway failures. All
resources and process samples belong to the caller's disposable test directory.
"""
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import statistics
import threading
import time


def qualify(api, preview, root, daemon, preview_port, token, endpoint, signal_record):
    results = {'max_sandboxes': 2, 'guest_memory_mib': 512, 'connection_ramp': []}
    stop = threading.Event()
    peaks = {}
    previous_cpu = {}
    ticks_per_second = os.sysconf('SC_CLK_TCK')
    def sample():
        while not stop.is_set():
            processes = [('daemon', daemon.pid)]
            for record in (root/'data'/'sandboxes').glob('*/**/*state.json'):
                try:
                    worker = json.loads(record.read_text())
                    processes.append((record.parent.name+'/'+record.name, worker['pid']))
                except (OSError, ValueError, KeyError):
                    continue
            for name, pid in processes:
                try:
                    status = Path(f'/proc/{pid}/status').read_text().splitlines()
                    values = {k: int(next(line for line in status if line.startswith(k+':')).split()[1])
                              for k in ['VmRSS', 'Threads']}
                    values['fds'] = len(list(Path(f'/proc/{pid}/fd').iterdir()))
                    stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
                    ticks, now = int(stat[11])+int(stat[12]), time.monotonic()
                    if pid in previous_cpu:
                        old_ticks, old_time = previous_cpu[pid]
                        values['cpu_percent_one_core'] = round(100*(ticks-old_ticks)/ticks_per_second/(now-old_time), 1)
                    previous_cpu[pid] = (ticks, now)
                    previous = peaks.setdefault(name, {})
                    for key, value in values.items(): previous[key] = max(previous.get(key, 0), value)
                except (OSError, StopIteration, ValueError):
                    continue
            time.sleep(0.2)
    monitor = threading.Thread(target=sample)
    monitor.start()
    def exec_guest(code, id='access-a'):
        out = api('POST', f'/sandboxes/{id}/exec', {'argv':['python3','-c',code]})
        assert out['exit_code'] == 0, out
        return out
    try:
        assert len(api('GET', '/sandboxes')['sandboxes']) == 2
        def guest_threads():
            out = api('POST','/sandboxes/access-a/exec',{'argv':['sh','-c',
                'mkdir -p /proc; mount -t proc proc /proc 2>/dev/null; grep Threads /proc/1/status']})
            assert out['exit_code'] == 0, out
            count = int(out['stdout'].split()[1])
            assert count > 0
            return count
        before_threads = guest_threads()
        vm = json.loads((root/'data'/'sandboxes'/'access-a'/'state.json').read_text())
        vm_fds_before = len(list(Path(f'/proc/{vm["pid"]}/fd').iterdir()))
        results['forge_threads_before_churn'] = before_threads
        offload = api('POST','/sandboxes/access-a/exec',{'argv':['/usr/sbin/ethtool','-k','eth0']})
        assert offload['exit_code'] == 0, offload
        text = offload['stdout']
        for feature in ['tx-checksumming', 'tcp-segmentation-offload']:
            assert feature+': off' in text, text
        results['offloads'] = text
        # Independent requests continuously open and close tunnels. A modest
        # sleep bounds request rate even on faster hardware.
        for connections in [2, 8, 16]:
            deadline = time.monotonic()+5
            def client(_):
                samples = []
                while time.monotonic() < deadline:
                    start = time.monotonic()
                    status, _, _ = preview()
                    assert status == 200, status
                    samples.append((time.monotonic()-start)*1000)
                    time.sleep(0.025)
                return samples
            with ThreadPoolExecutor(max_workers=connections) as pool:
                samples = [v for batch in pool.map(client, range(connections)) for v in batch]
            samples.sort()
            results['connection_ramp'].append({'connections':connections,'requests':len(samples),
                'p50_ms':round(statistics.median(samples),2),
                'p95_ms':round(samples[int((len(samples)-1)*0.95)],2),'max_ms':round(max(samples),2)})
        # The outbound path exercises the gateway separately from vsock previews.
        for connections in [2, 8, 16]:
            exec_guest(f'''
from concurrent.futures import ThreadPoolExecutor
import urllib.request
url='http://{endpoint[0]}:{endpoint[1]}/'
def run(_):
 for _ in range(8):
  with urllib.request.urlopen(url, timeout=5) as r: assert r.read()==b'PRIVATE-OK'
with ThreadPoolExecutor(max_workers={connections}) as p: list(p.map(run,range({connections})))
''')
        results['outbound_requests'] = (2+8+16)*8
        # Body forwarding must release its permit when the client disappears,
        # even after a large guest response stalls on backpressure.
        baseline_fds = len(list(Path(f'/proc/{daemon.pid}/fd').iterdir()))
        clients = []
        try:
            for _ in range(2):
                c = socket.create_connection(('127.0.0.1', preview_port), timeout=5)
                c.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                host = 'access-a'.encode().hex()+f'--18080.preview.localhost:{preview_port}'
                c.sendall(f'GET /bulk HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\r\n'.encode())
                clients.append(c)
            time.sleep(2)
            response = preview()
            assert response[0] == 200, response
        finally:
            for c in clients: c.close()
        deadline = time.monotonic()+8
        while len(list(Path(f'/proc/{daemon.pid}/fd').iterdir())) > baseline_fds+2:
            assert time.monotonic() < deadline, 'slow-reader descriptors leaked'
            time.sleep(0.1)
        results['slow_readers'] = 2
        # Resume a large response and verify every byte. This exercises partial
        # Unix writes/credit updates, not only short request/response handshakes.
        c = http.client.HTTPConnection('127.0.0.1', preview_port, timeout=10)
        c.request('GET','/bulk',headers={'Host':host,'Authorization':'Bearer '+token})
        response = c.getresponse()
        assert response.status == 200, response.status
        total = 0
        while chunk := response.read(65536):
            assert chunk == b'x'*len(chunk)
            total += len(chunk)
        c.close()
        assert total == 33554432, total
        results['verified_preview_bytes'] = total
        # Verified pidfd signalling; never signal a PID read from an unchecked file.
        records = root/'data'/'sandboxes'
        def record(id):
            paths = list((records/id).rglob('net-state.json'))
            assert len(paths) == 1, paths
            return json.loads(paths[0].read_text())
        peer = record('access-b')
        recovery = []
        for _ in range(3):
            old = record('access-a')
            assert signal_record(old, signal.SIGKILL)
            start = time.monotonic()
            deadline = start+15
            while True:
                assert record('access-b')['pid'] == peer['pid'], 'peer gateway replaced'
                assert api('POST','/sandboxes/access-b/exec',{'argv':['true']})['exit_code'] == 0
                if record('access-a')['pid'] != old['pid']:
                    out = api('POST','/sandboxes/access-a/exec',{'argv':['curl','-fsS','--max-time','2',f'http://{endpoint[0]}:{endpoint[1]}/']})
                    if out['exit_code'] == 0: break
                assert time.monotonic() < deadline, 'gateway recovery deadline'
                time.sleep(0.1)
            recovery.append(round(time.monotonic()-start,2))
            response = preview()
            assert response[0] == 200, response
        results['gateway_recovery_seconds'] = recovery
        # Guest forge tunnel threads must return after closed/slow connections.
        # Write timeout is 30s; this checks cleanup rather than instant teardown.
        exec_guest("import time; time.sleep(31)")
        results['forge_threads_after_churn'] = guest_threads()
        assert results['forge_threads_after_churn'] <= before_threads+2, results
        results['vmm_fds_before_churn'] = vm_fds_before
        results['vmm_fds_after_churn'] = len(list(Path(f'/proc/{vm["pid"]}/fd').iterdir()))
        assert results['vmm_fds_after_churn'] <= vm_fds_before+8, results

    finally:
        stop.set(); monitor.join(timeout=2)
    results['peak_process_resources'] = peaks
    return results
