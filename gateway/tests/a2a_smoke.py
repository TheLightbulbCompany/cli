#!/usr/bin/env python3
"""Independent interoperability smoke test using official a2a-sdk==1.3.0.

Run with the SDK environment's Python, after building the Rust binary:
  python tests/a2a_smoke.py --binary target/debug/lightbulb-gateway
The test starts isolated real supplier/gateway processes using e2e.py's fixture.
It uses the official SDK for discovery, request serialization, response parsing,
task polling, listing, and the unauthorized-task check; no hand-written RPC calls.
"""
import argparse
import asyncio
from importlib.metadata import version
import json
import os
from pathlib import Path
import time
import uuid

import httpx
from google.protobuf.json_format import ParseDict
from a2a.client import A2ACardResolver, ClientConfig, ClientFactory
from a2a.types import GetTaskRequest, ListTasksRequest, SendMessageRequest, TaskState
from a2a.utils.errors import TaskNotFoundError

import e2e


async def send(client, request):
    events = [event async for event in client.send_message(request)]
    assert len(events) == 1 and events[0].HasField("task"), events
    return events[0].task


async def smoke(base):
    assert version("a2a-sdk") == "1.3.0", "Use the pinned independent SDK"
    async with httpx.AsyncClient(
        headers={"Authorization": "Bearer integration-buyer-token"},
        timeout=15, trust_env=False,
    ) as http, httpx.AsyncClient(
        headers={"Authorization": "Bearer integration-other-token"},
        timeout=15, trust_env=False,
    ) as other_http:
        card = await A2ACardResolver(httpx_client=http, base_url=base).get_agent_card()
        assert any(i.protocol_binding == "JSONRPC" and i.protocol_version == "1.0"
                   for i in card.supported_interfaces), card
        assert card.supported_interfaces[0].url == base + "/a2a"
        client = ClientFactory(ClientConfig(
            httpx_client=http, streaming=False,
            supported_protocol_bindings=["JSONRPC"],
        )).create(card)
        other = ClientFactory(ClientConfig(
            httpx_client=other_http, streaming=False,
            supported_protocol_bindings=["JSONRPC"],
        )).create(card)

        # ProtoJSON parsing here and the SDK client are independent of the Rust
        # server's model types. A DataPart carries our published demo input.
        request = ParseDict({
            "message": {
                "messageId": str(uuid.uuid4()),
                "role": "ROLE_USER",
                "parts": [{"data": {"scenario": "automatic", "mode": "scripted"}}],
            },
            "configuration": {"returnImmediately": True},
        }, SendMessageRequest())
        first = await send(client, request)
        task_id = first.id
        assert task_id and first.context_id

        # Repeating the same message must return the original task, not create
        # another transaction. This is separate from business commit dedup.
        retried = await send(client, request)
        assert retried.id == task_id, (task_id, retried.id)

        deadline = time.monotonic() + 20
        terminal = {
            TaskState.TASK_STATE_COMPLETED, TaskState.TASK_STATE_FAILED,
            TaskState.TASK_STATE_REJECTED, TaskState.TASK_STATE_CANCELED,
        }
        while True:
            task = await client.get_task(GetTaskRequest(id=task_id))
            if task.status.state in terminal:
                break
            assert time.monotonic() < deadline, "SDK GetTask did not reach terminal state"
            await asyncio.sleep(0.05)
        assert task.status.state == TaskState.TASK_STATE_COMPLETED, task
        assert len(task.artifacts) == 1, task

        listing = await client.list_tasks(ListTasksRequest(include_artifacts=True))
        assert [t.id for t in listing.tasks] == [task_id], listing
        assert listing.total_size == 1 and listing.next_page_token == "", listing

        try:
            await other.get_task(GetTaskRequest(id=task_id))
        except TaskNotFoundError:
            pass  # SDK mapped the server's -32001 to its typed exception.
        else:
            raise AssertionError("A different caller could read the buyer's task")
        other_listing = await other.list_tasks(ListTasksRequest())
        assert not other_listing.tasks and other_listing.total_size == 0, other_listing

        return {
            "sdk": "official Python a2a-sdk==1.3.0",
            "protocol": "A2A 1.0 JSONRPC",
            "checks": ["Agent Card discovery", "SendMessage", "GetTask completion",
                       "artifact decoding", "ListTasks", "message retry deduplication",
                       "cross-caller GetTask denied", "cross-caller list isolation"],
            "task_id": task_id,
            "status": "passed",
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/lightbulb-gateway")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).is_file():
        parser.error(f"Build the gateway first; binary missing: {binary}")
    os.environ["LIGHTBULB_BUYER_TOKEN"] = "integration-buyer-token"
    os.environ["LIGHTBULB_OTHER_TOKEN"] = "integration-other-token"
    fixture = e2e.GatewayHTTP
    fixture.binary = binary
    fixture.setUpClass()
    try:
        result = asyncio.run(smoke(fixture.base))
        print(json.dumps(result, indent=2))
    finally:
        fixture.tearDownClass()


if __name__ == "__main__":
    main()
