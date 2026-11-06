"""rfb-tap.py <x11vnc socket> <listen socket> <log>: passes bytes through unchanged, logging when each
FramebufferUpdateRequest went up and how much came down, so request spacing can be read at the relay."""
import asyncio, sys, time

target, listen, log = sys.argv[1:4]
SIZE = {0: 20, 3: 10, 4: 8, 5: 6}

async def up(r, w, out):
    buf, seen = b'', 0
    while data := await r.read(65536):
        w.write(data)
        await w.drain()
        now = time.time()
        if seen < 14:
            skip = min(14 - seen, len(data))
            seen += skip
            data = data[skip:]
        buf += data
        while buf:
            t = buf[0]
            n = 4 + 4 * int.from_bytes(buf[2:4], 'big') if t == 2 and len(buf) >= 4 else 8 + int.from_bytes(buf[4:8], 'big') if t == 6 and len(buf) >= 8 else SIZE.get(t)
            if n is None and t not in (2, 6):
                out.write(f'{now:.4f} unknown {t}\n')
                buf = b''
                break
            if n is None or len(buf) < n:
                break
            if t == 3:
                out.write(f'{now:.4f} req {buf[1]}\n')
            buf = buf[n:]
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
