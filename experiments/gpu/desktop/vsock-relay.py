#!/usr/bin/env python3
"""Single-client test bridge: guest vsock 1025 to a private WayVNC socket."""
import select
import socket

with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM) as listener:
    listener.bind((socket.VMADDR_CID_ANY, 1025))
    listener.listen(1)
    listener.settimeout(35)
    peer, _ = listener.accept()
    with peer, socket.socket(socket.AF_UNIX) as vnc:
        vnc.connect('/run/user/1000/vnc.sock')
        peer.settimeout(10)
        vnc.settimeout(10)
        while True:
            ready, _, _ = select.select([peer, vnc], [], [], 10)
            if not ready:
                raise TimeoutError('Desktop test connection idle')
            for source in ready:
                data = source.recv(65536)
                if not data:
                    raise SystemExit(0)
                (vnc if source is peer else peer).sendall(data)
