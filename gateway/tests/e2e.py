#!/usr/bin/env python3
"""Behavioral HTTP checks against isolated real gateway and supplier processes."""
import argparse
import atexit
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request
import uuid

OWNER_TOKEN = "integration-owner-token"
BUYER_OWNER_TOKEN = "integration-buyer-owner"
SUPPLIER_OWNER_TOKEN = "integration-supplier-owner"
TERMINAL = {"completed", "rejected", "failed", "canceled"}


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class GatewayHTTP(unittest.TestCase):
    binary = None

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="lightbulb-e2e-")
        cls.root = Path(cls.tmp.name)
        cls.gateway_port = free_port()
        cls.supplier_port = free_port()
        while cls.supplier_port == cls.gateway_port:
            cls.supplier_port = free_port()
        cls.base = f"http://127.0.0.1:{cls.gateway_port}"
        cls.supplier_base = f"http://127.0.0.1:{cls.supplier_port}"
        cls.env = dict(os.environ, LIGHTBULB_OWNER_TOKEN=OWNER_TOKEN,
                       LIGHTBULB_BUYER_OWNER_TOKEN=BUYER_OWNER_TOKEN,
                       LIGHTBULB_SUPPLIER_OWNER_TOKEN=SUPPLIER_OWNER_TOKEN,
                       LIGHTBULB_EXECUTION_SECRET="local-demo-secret",
                       LIGHTBULB_SUPPLIER_TOKEN="integration-supplier-token",
                       LIGHTBULB_BUYER_TOKEN="integration-buyer-token",
                       LIGHTBULB_OTHER_TOKEN="integration-other-token")
        cls.logs = []
        cls.processes = {}
        atexit.register(cls.tearDownClass)
        try:
            cls.start_process("supplier")
            cls.start_process("gateway")
        except Exception:
            cls.tearDownClass()
            raise

    @classmethod
    def start_process(cls, role):
        log = open(cls.root / f"{role}-{len(cls.logs)}.log", "w+")
        cls.logs.append(log)
        port = cls.gateway_port if role == "gateway" else cls.supplier_port
        args = [cls.binary, "--role", role, "--port", str(port),
                "--data-dir", str(cls.root / role)]
        if role == "gateway":
            args += ["--supplier-url", cls.supplier_base]
        process = subprocess.Popen(args, stdout=log, stderr=log, env=cls.env)
        cls.processes[role] = process
        base = cls.base if role == "gateway" else cls.supplier_base
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if process.poll() is not None:
                log.flush()
                log.seek(0)
                raise RuntimeError(f"{role} exited early: {log.read()}")
            try:
                with urllib.request.urlopen(base + "/health", timeout=1) as response:
                    if response.status == 200:
                        return
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.1)
        raise RuntimeError(f"{role} did not become healthy")

    @classmethod
    def stop_process(cls, role):
        process = cls.processes.pop(role, None)
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)

    @classmethod
    def tearDownClass(cls):
        for role in list(getattr(cls, "processes", {})):
            cls.stop_process(role)
        for log in getattr(cls, "logs", []):
            log.close()
        if hasattr(cls, "tmp"):
            cls.tmp.cleanup()

    def request(self, path, data=None, token=None):
        headers = {"Accept": "application/json"}
        if token is not None:
            headers["X-Owner-Token"] = token
        raw = None if data is None else json.dumps(data).encode()
        if raw is not None:
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.base + path, data=raw, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=5) as response:
                body = response.read()
                return response.status, json.loads(body) if body else None
        except urllib.error.HTTPError as error:
            body = error.read()
            try:
                payload = json.loads(body)
            except json.JSONDecodeError:
                payload = body.decode(errors="replace")
            return error.code, payload

    def supplier_state(self):
        req = urllib.request.Request(self.supplier_base + "/api/state", headers={
            "Authorization": "Bearer integration-supplier-token"})
        with urllib.request.urlopen(req, timeout=5) as response:
            self.assertEqual(response.status, 200)
            return json.load(response)

    def rpc(self, method, params, supplier=False, token=None):
        base = self.supplier_base if supplier else self.base
        token = token or ("integration-supplier-token" if supplier else "integration-buyer-token")
        payload = {"jsonrpc": "2.0", "id": str(uuid.uuid4()),
                   "method": method, "params": params}
        req = urllib.request.Request(base + "/a2a", data=json.dumps(payload).encode(),
            headers={"Content-Type": "application/json", "A2A-Version": "1.0",
                     "Authorization": "Bearer " + token})
        with urllib.request.urlopen(req, timeout=5) as response:
            self.assertEqual(response.status, 200)
            return json.load(response)

    def send_rpc(self, action, supplier=False, token=None):
        message = {"messageId": str(uuid.uuid4()), "role": "ROLE_USER",
                   "parts": [{"data": action}]}
        if supplier and "task_id" in action:
            message["contextId"] = action["task_id"]
        return self.rpc("SendMessage", {"message": message}, supplier=supplier, token=token)

    def captured_commit(self, state, task_id):
        for event in state["events"]:
            if event["task_id"] != task_id or event["kind"] != "a2a_request":
                continue
            request = event["data"]
            parts = request.get("params", {}).get("message", {}).get("parts", [])
            if any(p.get("data", {}).get("action") == "commit_order" for p in parts):
                return json.loads(json.dumps(request))
        self.fail("No real signed commit request recorded for task " + task_id)


    def state(self):
        code, payload = self.request("/api/state")
        self.assertEqual(code, 200, payload)
        return payload

    def wait_for(self, predicate, description="condition", timeout=20):
        deadline = time.monotonic() + timeout
        state = None
        while time.monotonic() < deadline:
            state = self.state()
            if predicate(state):
                return state
            time.sleep(0.05)
        self.fail(f"Timed out waiting for {description}: {json.dumps(state, indent=2)}")

    def setUp(self):
        self.wait_for(lambda s: all(t["status"] != "working" for t in s["tasks"]),
                      "no working tasks before reset")
        code, result = self.reset_when_idle()
        self.assertIn(code, (200, 204), result)
        self.wait_for(lambda s: not s["tasks"] and not s["approvals"] and not s["orders"],
                      "empty state after reset")

    def reset_when_idle(self):
        # Persisted terminal status can become visible just before the in-flight
        # worker drops its guard. Honor the API's explicit busy response.
        deadline = time.monotonic() + 5
        while True:
            code, result = self.request("/api/reset", {}, OWNER_TOKEN)
            if code != 409 or "active tasks" not in str(result) or time.monotonic() >= deadline:
                return code, result
            time.sleep(0.05)

    def start_demo(self, scenario):
        code, result = self.request("/api/demo", {"scenario": scenario, "mode": "scripted"})
        self.assertIn(code, (200, 201, 202), result)
        self.assertIn("task_id", result)
        return result["task_id"]

    def task(self, state, task_id):
        return next(t for t in state["tasks"] if t["id"] == task_id)

    def wait_status(self, task_id, statuses):
        return self.wait_for(lambda s: any(t["id"] == task_id and t["status"] in statuses
                                           for t in s["tasks"]),
                             f"task {task_id} in {statuses}")

    def held(self, scenario="approval"):
        task_id = self.start_demo(scenario)
        state = self.wait_status(task_id, {"auth_required"})
        approvals = [a for a in state["approvals"]
                     if a["task_id"] == task_id and a["status"] == "pending"]
        self.assertEqual(len(approvals), 1, state)
        self.assertEqual(state["orders"], [], "Approval-pending task must not execute")
        self.assertFalse(self.supplier_state()["orders"], "Supplier must have no order before approval")
        return task_id, approvals[0]

    def answer(self, approval, decision="approve", owner=None, token="__owner__"):
        if token == "__owner__":
            token = BUYER_OWNER_TOKEN if approval["owner"] == "buyer" else SUPPLIER_OWNER_TOKEN
        return self.request("/api/approvals/" + approval["id"],
                            {"decision": decision, "owner": owner or approval["owner"]}, token)

    def test_health_and_scripted_label(self):
        code, _ = self.request("/health")
        self.assertEqual(code, 200)
        self.assertEqual(self.state()["mode"], "scripted")

    def test_automatic_completes_one_order(self):
        task_id = self.start_demo("automatic")
        state = self.wait_status(task_id, TERMINAL)
        self.assertEqual(self.task(state, task_id)["status"], "completed", state)
        orders = [o for o in state["orders"] if o["task_id"] == task_id]
        self.assertEqual(len(orders), 1, state)
        self.assertEqual(len(self.supplier_state()["orders"]), 1)

    def test_buyer_approval_controls_execution_and_cannot_replay(self):
        task_id, approval = self.held()
        self.assertEqual(approval["owner"], "buyer")
        code, result = self.answer(approval)
        self.assertIn(code, (200, 202), result)
        state = self.wait_status(task_id, TERMINAL)
        self.assertEqual(self.task(state, task_id)["status"], "completed", state)
        self.assertEqual(len(state["orders"]), 1, state)
        code, _ = self.answer(approval)
        self.assertEqual(code, 409)
        self.assertEqual(len(self.state()["orders"]), 1)

    def test_wrong_owner_cannot_approve(self):
        task_id, approval = self.held()
        code, _ = self.answer(approval, owner="supplier")
        self.assertEqual(code, 403)
        state = self.state()
        self.assertEqual(self.task(state, task_id)["status"], "auth_required")
        self.assertFalse(state["orders"])

    def test_missing_or_wrong_owner_token_cannot_approve(self):
        task_id, approval = self.held()
        for token in (None, "not-the-owner-token"):
            with self.subTest(token=token):
                code, _ = self.answer(approval, token=token)
                self.assertEqual(code, 401)
        state = self.state()
        self.assertEqual(self.task(state, task_id)["status"], "auth_required")
        self.assertFalse(state["orders"])

    def test_denial_never_creates_order(self):
        task_id, approval = self.held()
        code, result = self.answer(approval, "deny")
        self.assertIn(code, (200, 202), result)
        state = self.wait_status(task_id, TERMINAL)
        self.assertEqual(self.task(state, task_id)["status"], "rejected", state)
        self.assertFalse(state["orders"])

    def test_supplier_has_independent_approval(self):
        task_id, approval = self.held("supplier")
        self.assertEqual(approval["owner"], "supplier")
        code, _ = self.answer(approval, owner="buyer")
        self.assertEqual(code, 403)
        self.assertFalse(self.state()["orders"])
        code, result = self.answer(approval)
        self.assertIn(code, (200, 202), result)
        state = self.wait_status(task_id, TERMINAL)
        self.assertEqual(self.task(state, task_id)["status"], "completed", state)
        self.assertEqual(len(state["orders"]), 1, state)

    def test_protected_disclosure_is_rejected(self):
        task_id = self.start_demo("disclosure")
        state = self.wait_status(task_id, TERMINAL)
        task = self.task(state, task_id)
        self.assertEqual(task["status"], "rejected", state)
        self.assertFalse(state["orders"])
        self.assertFalse(task.get("result"), "Rejected disclosure must not return protected data")
        self.assertFalse(self.supplier_state()["orders"])

    def test_changed_terms_require_distinct_fresh_approval(self):
        task_id, first = self.held("changed_terms")
        code, result = self.answer(first)
        self.assertIn(code, (200, 202), result)
        state = self.wait_for(
            lambda s: any(a["task_id"] == task_id and a["id"] != first["id"]
                          and a["status"] == "pending" for a in s["approvals"]),
            "fresh approval for changed terms")
        second = next(a for a in state["approvals"]
                      if a["task_id"] == task_id and a["status"] == "pending")
        self.assertNotEqual(first["proposal_hash"], second["proposal_hash"])
        self.assertNotEqual(first["proposal"], second["proposal"])
        self.assertFalse(state["orders"], "Old approval cannot execute changed terms")
        code, _ = self.answer(first)
        self.assertEqual(code, 409)
        code, result = self.answer(second)
        self.assertIn(code, (200, 202), result)
        state = self.wait_status(task_id, TERMINAL)
        self.assertEqual(self.task(state, task_id)["status"], "completed", state)
        self.assertEqual(len(state["orders"]), 1)
        self.assertEqual(state["orders"][0]["proposal"], second["proposal"])

    def test_pending_approval_survives_gateway_restart(self):
        task_id, approval = self.held()
        self.stop_process("gateway")
        self.start_process("gateway")
        state = self.state()
        self.assertEqual(self.task(state, task_id)["status"], "auth_required")
        restored = next(a for a in state["approvals"] if a["id"] == approval["id"])
        self.assertEqual(restored["proposal_hash"], approval["proposal_hash"])
        code, result = self.answer(restored)
        self.assertIn(code, (200, 202), result)
        state = self.wait_status(task_id, TERMINAL)
        self.assertEqual(self.task(state, task_id)["status"], "completed", state)
        self.assertEqual(len(state["orders"]), 1)

    def test_owner_credentials_do_not_cross_organization_boundaries(self):
        for scenario, wrong_token in (("approval", SUPPLIER_OWNER_TOKEN),
                                      ("supplier", BUYER_OWNER_TOKEN)):
            with self.subTest(scenario=scenario):
                task_id, approval = self.held(scenario)
                # Even the correct owner name in the request cannot elevate the wrong credential.
                code, _ = self.answer(approval, token=wrong_token)
                self.assertEqual(code, 403)
                state = self.state()
                self.assertEqual(self.task(state, task_id)["status"], "auth_required")
                self.assertFalse(state["orders"])
                self.assertFalse(self.supplier_state()["orders"])
                code, result = self.answer(approval, "deny")
                self.assertIn(code, (200, 202), result)
                self.wait_status(task_id, {"rejected"})

    def test_supplier_rejects_missing_and_forged_execution_permits(self):
        proposal = {"task_id": "unapproved-direct-order", "buyer": "buyer",
                    "supplier": "supplier", "sku": "ORIGINAL-CASE", "quantity": 20,
                    "delivery": "Friday", "currency": "USD", "total_cents": 40000,
                    "substitution": False, "discount_percent": 0, "quote_version": 1,
                    "supplier_epoch": self.supplier_state()["epoch"]}
        for permit in (None, {"claims": {"task_id": proposal["task_id"],
                                        "proposal_hash": "made-up-hash",
                                        "expires_at": int(time.time()) + 300},
                              "signature": "00" * 32}):
            with self.subTest(permit=permit):
                request = {"action": "commit_order", "task_id": proposal["task_id"],
                           "proposal": proposal}
                if permit is not None:
                    request["permit"] = permit
                result = self.send_rpc(request, supplier=True)
                self.assertIn("error", result, result)
                self.assertFalse(self.supplier_state()["orders"],
                                 "Even an authenticated sender cannot commit without a valid permit")

    def test_denied_approval_cannot_be_approved_later(self):
        task_id, approval = self.held()
        code, result = self.answer(approval, "deny")
        self.assertIn(code, (200, 202), result)
        self.wait_status(task_id, {"rejected"})
        code, _ = self.answer(approval, "approve")
        self.assertEqual(code, 409)
        self.assertFalse(self.state()["orders"])
        self.assertFalse(self.supplier_state()["orders"])

    def test_approving_one_task_does_not_authorize_another(self):
        first_id, first = self.held()
        second_id, second = self.held()
        self.assertNotEqual(first["proposal_hash"], second["proposal_hash"])
        code, result = self.answer(first)
        self.assertIn(code, (200, 202), result)
        state = self.wait_status(first_id, TERMINAL)
        self.assertEqual(self.task(state, first_id)["status"], "completed", state)
        self.assertEqual(self.task(state, second_id)["status"], "auth_required", state)
        self.assertEqual([o["task_id"] for o in state["orders"]], [first_id])
        self.assertEqual([o["task_id"] for o in self.supplier_state()["orders"]], [first_id])
        code, result = self.answer(second, "deny")
        self.assertIn(code, (200, 202), result)
        self.wait_status(second_id, {"rejected"})

    def test_a2a_task_can_be_canceled_while_waiting(self):
        result = self.send_rpc({"scenario": "approval", "mode": "scripted"})
        self.assertNotIn("error", result, result)
        task_id = result["result"]["task"]["id"]
        state = self.wait_status(task_id, {"auth_required"})
        pending = next(a for a in state["approvals"]
                       if a["task_id"] == task_id and a["status"] == "pending")
        observed = self.rpc("GetTask", {"id": task_id})
        self.assertEqual(observed["result"]["status"]["state"], "TASK_STATE_AUTH_REQUIRED")
        canceled = self.rpc("CancelTask", {"id": task_id})
        self.assertNotIn("error", canceled, canceled)
        self.assertEqual(canceled["result"]["status"]["state"], "TASK_STATE_CANCELED")
        code, _ = self.answer(pending)
        self.assertEqual(code, 409)
        self.assertFalse(self.state()["orders"])
        self.assertFalse(self.supplier_state()["orders"])

    def test_signed_execution_permit_cannot_be_rebound_to_another_task(self):
        task_id = self.start_demo("automatic")
        state = self.wait_status(task_id, {"completed"})
        commit = self.captured_commit(state, task_id)
        message = commit["params"]["message"]
        # Reuse genuine signed claims while redirecting the surrounding business task.
        message["messageId"] = str(uuid.uuid4())
        message["contextId"] = "different-business-task"
        message["parts"][0]["data"]["task_id"] = "different-business-task"
        result = self.rpc("SendMessage", {"message": message}, supplier=True)
        self.assertIn("error", result, result)
        self.assertEqual([o["task_id"] for o in self.supplier_state()["orders"]], [task_id])
        # Matching business fields still cannot excuse an unrelated protocol context.
        original = self.captured_commit(state, task_id)["params"]["message"]
        original["messageId"] = str(uuid.uuid4())
        original["contextId"] = "unrelated-protocol-context"
        result = self.rpc("SendMessage", {"message": original}, supplier=True)
        self.assertIn("error", result, result)
        self.assertEqual(len(self.supplier_state()["orders"]), 1)

    def test_reset_invalidates_previously_signed_execution_permits(self):
        task_id = self.start_demo("automatic")
        state = self.wait_status(task_id, {"completed"})
        commit = self.captured_commit(state, task_id)
        previous_epoch = self.supplier_state()["epoch"]
        code, result = self.reset_when_idle()
        self.assertEqual(code, 200, result)
        self.assertNotEqual(self.supplier_state()["epoch"], previous_epoch)
        self.assertFalse(self.supplier_state()["orders"])
        commit["params"]["message"]["messageId"] = str(uuid.uuid4())
        result = self.rpc("SendMessage", commit["params"], supplier=True)
        self.assertIn("error", result, result)
        self.assertFalse(self.supplier_state()["orders"],
                         "Reset must not let a captured valid old permit recreate an order")

    def test_lost_commit_response_recovers_after_restart_without_duplicate_order(self):
        old = self.env.get("LIGHTBULB_INJECT_LOST_COMMIT_RESPONSE")
        try:
            self.stop_process("gateway")
            self.env["LIGHTBULB_INJECT_LOST_COMMIT_RESPONSE"] = "1"
            self.start_process("gateway")
            task_id = self.start_demo("automatic")
            state = self.wait_status(task_id, {"unknown"})
            self.assertEqual(self.task(state, task_id)["execution_state"], "dispatched")
            self.assertFalse(state["orders"], "Gateway must not claim an unobserved confirmation")
            orders_before = self.supplier_state()["orders"]
            self.assertEqual(len(orders_before), 1, "Supplier committed before the response was lost")
            self.stop_process("gateway")
            self.start_process("gateway")
            state = self.wait_status(task_id, {"completed"})
            self.assertEqual(self.task(state, task_id)["execution_state"], "confirmed")
            self.assertEqual(self.supplier_state()["orders"], orders_before)
            self.assertEqual(len(state["orders"]), 1)
            commits = [e for e in state["events"] if e["task_id"] == task_id
                       and e["kind"] == "a2a_request"
                       and any(p.get("data", {}).get("action") == "commit_order"
                               for p in e["data"]["params"]["message"]["parts"])]
            self.assertEqual(len(commits), 1, "Recovery should query the supplier ledger before retrying a commit")
        finally:
            self.stop_process("gateway")
            if old is None:
                self.env.pop("LIGHTBULB_INJECT_LOST_COMMIT_RESPONSE", None)
            else:
                self.env["LIGHTBULB_INJECT_LOST_COMMIT_RESPONSE"] = old
            self.start_process("gateway")

    def test_reset_requires_owner_token(self):
        task_id = self.start_demo("automatic")
        self.wait_status(task_id, TERMINAL)
        code, _ = self.request("/api/reset", {})
        self.assertEqual(code, 401)
        self.assertEqual(len(self.state()["orders"]), 1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/lightbulb-gateway")
    args, extra = parser.parse_known_args()
    GatewayHTTP.binary = str(Path(args.binary).resolve())
    if not Path(GatewayHTTP.binary).is_file():
        parser.error(f"Build the gateway first; binary missing: {GatewayHTTP.binary}")
    unittest.main(argv=[__file__] + extra, verbosity=2)
