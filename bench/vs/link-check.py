"""link-check.py <shaper.py> <seconds>: sustained rate of a bulk stream through bench/shaper.py, as plain TCP
and through websockify as noVNC's path has it, at each link setting. Bytes on the wire, after a 3 s warm-up."""
import base64, os, socket, subprocess, sys, threading, time

shaper, seconds = sys.argv[1], float(sys.argv[2])
SOCK = '/tmp/vs-bulk.sock'

def feed(c):
    chunk = b'\xa5' * 65536
    try:
        while True:
            c.sendall(chunk)
    except OSError:
        pass

def serve(s):
    while True:
        c, _ = s.accept()
        threading.Thread(target=feed, args=(c,), daemon=True).start()

def wait_port(port):
    for _ in range(100):
        try:
            socket.create_connection(('127.0.0.1', port), 1).close()
            return
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f'nothing on {port}')

def measure(port, ws):
    c = socket.create_connection(('127.0.0.1', port))
    if ws:
        key = base64.b64encode(os.urandom(16)).decode()
        c.sendall(f'GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n'.encode())
        head = b''
        while b'\r\n\r\n' not in head:
            head += c.recv(4096)
        assert b' 101 ' in head.split(b'\r\n')[0], head[:80]
    start, n, t0 = time.time(), 0, None
    while time.time() - start < seconds + 3:
        d = c.recv(1 << 20)
        if not d:
            break
        if time.time() - start >= 3:
            t0 = t0 or time.time()
            n += len(d)
    c.close()
    return n * 8 / (time.time() - t0) / 1e6

tcp = socket.socket()
tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
tcp.bind(('127.0.0.1', 7101))
tcp.listen(8)
threading.Thread(target=serve, args=(tcp,), daemon=True).start()
if os.path.exists(SOCK):
    os.remove(SOCK)
uds = socket.socket(socket.AF_UNIX)
uds.bind(SOCK)
uds.listen(8)
threading.Thread(target=serve, args=(uds,), daemon=True).start()
def stop(procs):
    for p in procs:
        try:
            os.killpg(p.pid, 15)
        except ProcessLookupError:
            pass
    for p in procs:
        p.wait()

ws = subprocess.Popen(['websockify', f'--unix-target={SOCK}', '127.0.0.1:7102'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
procs = []
try:
    wait_port(7102)
    for mbps in (20, 2):
        procs = [subprocess.Popen(['python3', shaper, str(l), f'127.0.0.1:{t}', str(mbps), '50', '250000'], start_new_session=True) for l, t in ((7201, 7101), (7202, 7102))]
        wait_port(7201)
        wait_port(7202)
        for i in range(3):
            print(f'LINK {mbps} Mbit/s tcp {measure(7201, False):.2f} websocket {measure(7202, True):.2f}', flush=True)
        stop(procs)
        procs = []
finally:
    stop(procs + [ws])
