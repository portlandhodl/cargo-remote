#!/usr/bin/env python3
"""TCP proxy that adds a fixed one-way delay without throttling throughput.

Usage: latency_proxy.py <listen_port> <target_port> <delay_ms_each_way>

Each chunk is delivered exactly delay_ms after it was read (queued, then
written when due), so a protocol round trip costs ~2*delay_ms extra — like a
real WAN link — while bulk throughput is not capped.
"""
import asyncio
import sys
import time


async def pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, delay: float):
    queue: asyncio.Queue[tuple[float, bytes | None]] = asyncio.Queue()

    async def read_side():
        try:
            while True:
                data = await reader.read(1 << 16)
                if not data:
                    break
                queue.put_nowait((time.monotonic() + delay, data))
        except (ConnectionResetError, BrokenPipeError):
            pass
        finally:
            queue.put_nowait((time.monotonic(), None))

    async def write_side():
        try:
            while True:
                due, data = await queue.get()
                if data is None:
                    break
                wait = due - time.monotonic()
                if wait > 0:
                    await asyncio.sleep(wait)
                writer.write(data)
                await writer.drain()
        except (ConnectionResetError, BrokenPipeError):
            pass
        finally:
            try:
                writer.close()
            except Exception:
                pass

    await asyncio.gather(read_side(), write_side())


async def main() -> None:
    listen_port = int(sys.argv[1])
    target_port = int(sys.argv[2])
    delay = float(sys.argv[3]) / 1000.0

    async def handle(client_r: asyncio.StreamReader, client_w: asyncio.StreamWriter):
        target_r, target_w = await asyncio.open_connection("127.0.0.1", target_port)
        await asyncio.gather(
            pipe(client_r, target_w, delay),
            pipe(target_r, client_w, delay),
        )

    server = await asyncio.start_server(handle, "127.0.0.1", listen_port)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
