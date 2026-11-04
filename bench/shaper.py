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
        while True:
            t, data = await q.get()
            if data is None:
                break
            dt = t - time.monotonic()
            if dt > 0:
                await asyncio.sleep(dt)
            writer.write(data)
            await writer.drain()
            nonlocal queued
            queued -= len(data)
            room.set()
        writer.close()
    task = asyncio.create_task(sender())
    last = time.monotonic()
    while True:
        while cap and queued > cap:
            room.clear()
            await room.wait()
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

async def handle(cr, cw):
    sr, sw = await asyncio.open_connection(th, int(tp))
    await asyncio.gather(pipe(cr, sw, 0, rtt / 2000), pipe(sr, cw, mbps * 1e6, rtt / 2000, cap), return_exceptions=True)

async def main():
    srv = await asyncio.start_server(handle, '127.0.0.1', listen)
    async with srv:
        await srv.serve_forever()
asyncio.run(main())
