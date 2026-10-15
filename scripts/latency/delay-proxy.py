#!/usr/bin/env python3
"""A TCP proxy that adds network latency, for measuring Bellows against a far server.

Each direction is delayed by half the round trip, so a request and its response
together cost one RTT. A new connection's first client bytes wait a further
`--handshake-rtts` round trips, standing in for TCP's handshake (1) plus TLS
1.3's (1) that a real HTTPS server costs and loopback does not. Bandwidth is
unlimited; only latency is modelled. No root needed (unlike `tc netem`).

  delay-proxy.py --listen 127.0.0.1:7900 --upstream 127.0.0.1:7878 --rtt-ms 100

On SIGTERM or SIGINT it writes {"connections": N, "bytes_up": .., "bytes_down": ..}
to `--stats` (when given) and exits.
"""

import argparse
import asyncio
import json
import signal
import time

CHUNK = 1 << 16


class Stats:
    connections = 0
    bytes_up = 0
    bytes_down = 0


async def pump(reader, writer, delay, first_extra, stats, upward):
    queue = asyncio.Queue()

    async def read():
        first = True
        while True:
            try:
                data = await reader.read(CHUNK)
            except (ConnectionError, OSError):
                data = b""
            due = time.monotonic() + delay + (first_extra if first else 0.0)
            first = False
            await queue.put((due, data))
            if not data:
                return

    async def write():
        while True:
            due, data = await queue.get()
            wait = due - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            if not data:
                try:
                    if writer.can_write_eof():
                        writer.write_eof()
                except (ConnectionError, OSError):
                    pass
                return
            if upward:
                stats.bytes_up += len(data)
            else:
                stats.bytes_down += len(data)
            try:
                writer.write(data)
                await writer.drain()
            except (ConnectionError, OSError):
                return

    await asyncio.gather(read(), write())


async def handle(client_reader, client_writer, args, stats):
    stats.connections += 1
    host, port = args.upstream.rsplit(":", 1)
    try:
        up_reader, up_writer = await asyncio.open_connection(host, int(port))
    except OSError:
        client_writer.close()
        return
    one_way = args.rtt_ms / 2000.0
    handshake = args.handshake_rtts * args.rtt_ms / 1000.0
    try:
        await asyncio.gather(
            pump(client_reader, up_writer, one_way, handshake, stats, True),
            pump(up_reader, client_writer, one_way, 0.0, stats, False),
        )
    finally:
        for writer in (up_writer, client_writer):
            try:
                writer.close()
            except (ConnectionError, OSError):
                pass


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", required=True)
    parser.add_argument("--upstream", required=True)
    parser.add_argument("--rtt-ms", type=float, required=True)
    parser.add_argument("--handshake-rtts", type=float, default=2.0)
    parser.add_argument("--stats")
    args = parser.parse_args()
    stats = Stats()
    host, port = args.listen.rsplit(":", 1)
    server = await asyncio.start_server(
        lambda r, w: handle(r, w, args, stats), host, int(port), limit=CHUNK
    )
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    print(f"delay-proxy {args.listen} -> {args.upstream} rtt {args.rtt_ms} ms", flush=True)
    async with server:
        await stop.wait()
    if args.stats:
        with open(args.stats, "w") as out:
            json.dump(
                {
                    "connections": stats.connections,
                    "bytes_up": stats.bytes_up,
                    "bytes_down": stats.bytes_down,
                },
                out,
            )


if __name__ == "__main__":
    asyncio.run(main())
