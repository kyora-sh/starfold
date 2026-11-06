"""rfb-tap.py <x11vnc socket> <listen socket> <log>: passes bytes through unchanged, logging when each
FramebufferUpdateRequest went up (parsed from the client stream after the RFB 3.8 no-auth handshake) and how much came down,
so request spacing can be read at the relay."""
import asyncio, sys, time

target, listen, log = sys.argv[1:4]

HANDSHAKE = 12 + 1 + 1
FIXED = {0: 20, 3: 10, 4: 8, 5: 6, 150: 10, 250: 4, 255: 12}

def length(buf, i):
    t = buf[i]
    if t in FIXED:
        return FIXED[t]
    if t == 2 and len(buf) - i >= 4:
        return 4 + 4 * int.from_bytes(buf[i + 2:i + 4], 'big')
    if t == 6 and len(buf) - i >= 8:
        return 8 + abs(int.from_bytes(buf[i + 4:i + 8], 'big', signed=True))
    if t == 248 and len(buf) - i >= 9:
        return 9 + buf[i + 8]
    if t == 251 and len(buf) - i >= 7:
        return 8 + 16 * buf[i + 6]
    return None if t in (2, 6, 248, 251) else -1

async def up(r, w, out):
    buf, skip, lost = bytearray(), HANDSHAKE, False
    while data := await r.read(65536):
        w.write(data)
        await w.drain()
        if lost:
            continue
        now = time.time()
        buf += data
        if skip:
            n = min(skip, len(buf))
            del buf[:n]
            skip -= n
        i = 0
        while i < len(buf) and (n := length(buf, i)) is not None and len(buf) - i >= n:
            if n < 0:
                out.write(f'{now:.4f} desync {buf[i]}\n')
                lost = True
                break
            if buf[i] == 3:
                out.write(f'{now:.4f} req {buf[i + 1]}\n')
            i += n
        del buf[:i]
    w.close()

async def down(r, w, out):
    while data := await r.read(65536):
        out.write(f'{time.time():.4f} down {len(data)}\n')
        w.write(data)
        await w.drain()
    w.close()

async def handle(cr, cw):
    sr, sw = await asyncio.open_unix_connection(target)
    with open(log, 'a') as out:
        out.write('CONNECT\n')
        await asyncio.gather(up(cr, sw, out), down(sr, cw, out), return_exceptions=True)

async def main():
    async with await asyncio.start_unix_server(handle, listen) as srv:
        await srv.serve_forever()
asyncio.run(main())
