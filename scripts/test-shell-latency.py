#!/usr/bin/env python3
"""Live PTY gate against one EXISTING sandbox; never creates a VM.
Usage: test-shell-latency.py /absolute/path/to/ahvm SANDBOX [MAX_P95_MS]
Measures real keystroke echo over the configured remote host, checks Bash,
and detaches. Delete the test sandbox afterwards to remove its shell session.
"""
import os,pty,select,time,signal,statistics,sys
binary=sys.argv[1]; sandbox=sys.argv[2]
pid,fd=pty.fork()
if pid==0:
 os.execve(binary,[binary,'shell',sandbox],{**os.environ,'AHVM_NO_UPDATE_CHECK':'1'})
buffer=b''
def until(marker,timeout=15):
 global buffer
 end=time.monotonic()+timeout
 while marker not in buffer:
  if time.monotonic()>end: raise AssertionError(repr(buffer[-1000:]))
  if select.select([fd],[],[],.1)[0]: buffer+=os.read(fd,65536)
 buffer=buffer.split(marker,1)[1]
def drain():
 global buffer
 while select.select([fd],[],[],.15)[0]: os.read(fd,65536)
 buffer=b''
try:
 until(b'Session ')
 os.write(fd,b"printf '\\122\\105\\101\\104\\131\\n'; echo SHELL:$0\n")
 until(b'READY\r\n'); time.sleep(.7); drain()
 samples=[]
 for c in b'abcdefghijklmnopqrst':
  begin=time.monotonic();os.write(fd,bytes([c]));until(bytes([c]));samples.append((time.monotonic()-begin)*1000)
  time.sleep(.075)
 os.write(fd,b'\x03');time.sleep(.6);drain()
 os.write(fd,b"printf '\\123\\110\\105\\114\\114\\075'; echo $0\n")
 until(b'SHELL=');until(b'/bin/bash\r\n')
 os.write(fd,b'\x1d')
 p95=sorted(samples)[18]
 print(f'Bash keystroke echo: median {statistics.median(samples):.1f} ms, p95 {p95:.1f} ms, max {max(samples):.1f} ms')
 assert p95 < float(sys.argv[3] if len(sys.argv)>3 else 200), 'Interactive latency exceeds budget'
 for _ in range(50):
  done,status=os.waitpid(pid,os.WNOHANG)
  if done:
   assert os.waitstatus_to_exitcode(status)==0, 'Unclean detach'
   pid=0;break
  time.sleep(.1)
 else: raise AssertionError('Shell did not detach')
finally:
 try:
  if pid: os.kill(pid,signal.SIGTERM)
 except ProcessLookupError:pass
 os.close(fd)
