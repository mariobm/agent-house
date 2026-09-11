#!/usr/bin/env python3
"""Private VNC transport. Four clients maximum, no host/guest TCP listener."""
import select
import socket
import threading
import time
from pathlib import Path

slots = threading.BoundedSemaphore(4)

def relay(peer):
    try:
        with peer, socket.socket(socket.AF_UNIX) as vnc:
            vnc.connect('/run/user/1000/vnc.sock')
            peer.settimeout(10)
            vnc.settimeout(10)
            deadline = time.monotonic() + 3600
            while time.monotonic() < deadline:
                ready, _, _ = select.select([peer, vnc], [], [], 1)
                for source in ready:
                    data = source.recv(65536)
                    if not data:
                        return
                    (vnc if source is peer else peer).sendall(data)
    except OSError:
        pass
    finally:
        slots.release()

with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM) as listener:
    listener.bind((socket.VMADDR_CID_ANY, 1025))
    listener.listen(4)
    Path("/run/user/1000/ahvm-desktop-ready").touch()
    while True:
        peer, _ = listener.accept()
        if slots.acquire(blocking=False):
            threading.Thread(target=relay, args=(peer,), daemon=True).start()
        else:
            peer.close()
