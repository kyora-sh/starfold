"""rfb-tap.py <x11vnc socket> <listen socket> <log>: passes bytes through unchanged, logging when each
FramebufferUpdateRequest (one or more leading a client write) went up and how much came down, so request spacing can be read at the relay."""
import asyncio, sys, time

target, listen, log = sys.argv[1:4]

async def up(r, w, out):
    while data := await r.read(65536):
        w.write(data)
        await w.drain()
        now = time.time()
        for i in range(0, len(data) - 9, 10):
            if data[i] != 3:
                break
            out.write(f'{now:.4f} req {data[i + 1]}\n')
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
