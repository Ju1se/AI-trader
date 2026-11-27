"""In-process pub/sub that feeds the SSE stream.

Good enough for one backend process. With several processes this needs a shared channel
(PostgreSQL LISTEN/NOTIFY or Redis); the publish/subscribe interface stays the same.
"""

import asyncio


class EventBus:
    def __init__(self) -> None:
        self._subscribers: set[asyncio.Queue] = set()
        self._loop: asyncio.AbstractEventLoop | None = None

    def bind(self, loop: asyncio.AbstractEventLoop) -> None:
        self._loop = loop

    def publish(self, event: dict) -> None:
        # Called from sync endpoints running in worker threads, so hop onto the loop.
        if self._loop is None or self._loop.is_closed():
            return
        for queue in list(self._subscribers):
            self._loop.call_soon_threadsafe(queue.put_nowait, event)

    def subscribe(self) -> asyncio.Queue:
        queue: asyncio.Queue = asyncio.Queue()
        self._subscribers.add(queue)
        return queue

    def unsubscribe(self, queue: asyncio.Queue) -> None:
        self._subscribers.discard(queue)


bus = EventBus()
