"""TCP proxy emulating a bottleneck link: rate limit (Mbit/s) on server->client,
plus fixed one-way delay each way. Queue is unbounded (like a bufferbloated path) unless
max_queue_bytes is given: past it the proxy stops reading, so the sender feels backpressure.
Usage: shaper.py <listen_port> <target_host:port> <mbps> <rtt_ms> [max_queue_bytes]"""
import asyncio, sys, time

listen, target, mbps, rtt = int(sys.argv[1]), sys.argv[2], float(sys.argv[3]), float(sys.argv[4])
cap = int(sys.argv[5]) if len(sys.argv) > 5 else 0
th, tp = target.split(':')

async def pipe(reader, writer, rate_bps, delay, cap=0):
    q = asyncio.Queue()
    queued = 0
    room = asyncio.Event()
    async def sender():
        nonlocal queued
        try:
            while True:
                t, data = await q.get()
                if data is None:
                    break
                dt = t - time.monotonic()
                if dt > 0:
                    await asyncio.sleep(dt)
                writer.write(data)
                await writer.drain()
                queued -= len(data)
                room.set()
        finally:
            writer.close()
    task = asyncio.create_task(sender())
    w = None
    try:
        last = time.monotonic()
        while True:
            while cap and queued > cap:
                room.clear()
                w = asyncio.ensure_future(room.wait())
                await asyncio.wait([w, task], return_when=asyncio.FIRST_COMPLETED)
                w.cancel()
                if task.done():
                    return await task
            data = await reader.read(8192 if cap else 65536)
            queued += len(data)
            if not data:
                await q.put((0, None)); break
            now = time.monotonic()
            if rate_bps:
                last = max(last, now) + len(data) * 8 / rate_bps
                t = last + delay
            else:
                t = now + delay
            await q.put((t, data))
        await task
    finally:
        for f in (task, w):
            if f and not f.done():
                f.cancel()
        await asyncio.gather(*(f for f in (task, w) if f), return_exceptions=True)

async def handle(cr, cw):
    sr, sw = await asyncio.open_connection(th, int(tp))
    up = asyncio.create_task(pipe(cr, sw, 0, rtt / 2000))
    down = asyncio.create_task(pipe(sr, cw, mbps * 1e6, rtt / 2000, cap))
    await asyncio.wait([up, down], return_when=asyncio.FIRST_EXCEPTION)
    for t in (up, down):
        t.cancel()
    await asyncio.gather(up, down, return_exceptions=True)
    sw.close()
    cw.close()

async def main():
    srv = await asyncio.start_server(handle, '127.0.0.1', listen)
    async with srv:
        await srv.serve_forever()
asyncio.run(main())
