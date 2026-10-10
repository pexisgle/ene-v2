import asyncio
import sys
from pathlib import Path

sys.path.insert(0, sys.argv[1])
import ene_provider


async def close_pending_response(broker):
    stalled = await broker.request({"method": "GET", "url": "stall", "headers": {}, "body": None})
    reader = asyncio.ensure_future(anext(stalled))
    await asyncio.sleep(0.05)
    await asyncio.wait_for(stalled.aclose(), timeout=1)
    try:
        await asyncio.wait_for(reader, timeout=1)
    except StopAsyncIteration as stopped:
        # The completed future must not retain the provider frame and its RTC senders.
        stopped.__traceback__ = None
    else:
        raise AssertionError("close must terminate the pending read")


class Provider:
    async def list_models(self, endpoint, options, broker):
        if options.get("fail"):
            raise ValueError("private provider diagnostic")
        response = await broker.request({
            "method": "GET", "url": endpoint, "headers": {}, "body": None,
        })
        assert response.status == 200
        assert response.headers == {"content-type": "text/plain"}
        text = (b"".join([chunk async for chunk in response])).decode()
        return [{"id": text, "name": text, "metadata": {
            "source": endpoint, "scope": "catalog", "context_window": 2**63,
            "max_input_tokens": None, "max_output_tokens": None,
            "input_modalities": None, "output_modalities": None,
            "supported_parameters": None, "field_sources": {},
        }}]

    async def generate(self, request, broker, deltas):
        assert request["max_tokens"] == 2**63
        assert request["options"] == {"nested": [True, None, {"text": "日本語"}]}
        await close_pending_response(broker)
        response = await broker.request({
            "method": "POST", "url": "binary", "headers": {}, "body": b"\x00\xff\n",
        })
        assert b"".join([chunk async for chunk in response]) == "日本語".encode()
        await response.aclose()
        try:
            await deltas.send("日本語" * 30000)
            if request["model"] == "cancel":
                await asyncio.sleep(3600)
            assert len(request["preamble"]) == 1100000
            return "stop"
        finally:
            if request["model"] == "cancel":
                Path(sys.argv[2]).write_text("cancelled")



async def main():
    await ene_provider.serve(Provider())


asyncio.run(main())
