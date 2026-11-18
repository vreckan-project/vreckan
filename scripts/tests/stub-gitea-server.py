#!/usr/bin/env python3
"""Stub Gitea actions API for scripts/tests/test-trigger-gitea.sh (ANT-1241).

Mode env (per server instance):
  STUB_PORT        — listen port (required)
  STUB_DISPATCH    — ok-200 | no-details-200 | ok-204 | fail-500
  STUB_LIST        — appears | never | busy-pool   (run list after dispatch)
  STUB_RUN         — success | failure | stall | started (single-run status over time:
                      success = completed immediately; failure = in_progress on
                      the first poll then completed/failure; stall = waiting
                      forever; started = in_progress forever, never completes)
  STUB_JOBS        — unassigned | assigned         (jobs of the new run)
"""
import datetime
import json
import os
import re
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(os.environ["STUB_PORT"])
MODE_DISPATCH = os.environ.get("STUB_DISPATCH", "ok-200")
# Optional: verify the Authorization header on the dispatch. The expected
# header is built from these (space-free) values; unset = no auth check.
import base64 as _b64

AUTH_USER = os.environ.get("STUB_AUTH_USER")
AUTH_PASS = os.environ.get("STUB_AUTH_PASS")
AUTH_TOKEN = os.environ.get("STUB_AUTH_TOKEN")


def expected_auth_header():
    if AUTH_USER is not None:
        return "Basic " + _b64.b64encode(f"{AUTH_USER}:{AUTH_PASS}".encode()).decode()
    if AUTH_TOKEN is not None:
        return "token " + AUTH_TOKEN
    return None
MODE_LIST = os.environ.get("STUB_LIST", "appears")
MODE_RUN = os.environ.get("STUB_RUN", "success")
MODE_JOBS = os.environ.get("STUB_JOBS", "unassigned")

REPO = "vreckan/vreckan-workflow"
WF = "build-test-rust.yml"
NEW_RUN_ID = 9001
OLD_RUN_ID = 9000
BUSY_RUN_ID = 9500

state = {"dispatched_at": None, "run_polls": 0}


def iso(epoch):
    return datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


def run_obj(rid, status, conclusion, created):
    return {
        "id": rid,
        "url": f"http://stub/api/v1/repos/{REPO}/actions/runs/{rid}",
        "html_url": f"http://stub/{REPO}/actions/runs/{rid}",
        "display_title": "stub run",
        "path": f"{WF}@refs/heads/main",
        "event": "workflow_dispatch",
        "run_attempt": 1,
        "run_number": rid - 1000,
        "head_sha": "0" * 40,
        "head_branch": "main",
        "status": status,
        "conclusion": conclusion,
        "actor": {"id": 1, "login": "stub"},
        "created_at": iso(created),
        "updated_at": iso(time.time()),
    }


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send(self, code, body=b""):
        self.send_response(code)
        if body:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_POST(self):
        try:
            self._post()
        except Exception as exc:
            self._send(500, json.dumps({"message": f"stub error: {exc}"}).encode())

    def _post(self):
        expected = expected_auth_header()
        if expected and self.headers.get("Authorization") != expected:
            self._send(401, json.dumps({"message": "Invalid OAuth2 token or basic auth credential"}).encode())
            return
        if self.path.startswith(f"/api/v1/repos/{REPO}/actions/workflows/{WF}/dispatches"):
            length = int(self.headers.get("Content-Length", 0))
            if length:
                self.rfile.read(length)
            if MODE_DISPATCH == "ok-200":
                # Same shape as Gitea's RunDetails struct (the real
                # 28.1.0 response for ?return_run_details=true).
                state["dispatched_at"] = time.time()
                self._send(
                    200,
                    json.dumps(
                        {
                            "workflow_run_id": NEW_RUN_ID,
                            "html_url": f"http://stub/{REPO}/actions/runs/{NEW_RUN_ID}",
                            "run_url": f"http://stub/api/v1/repos/{REPO}/actions/runs/{NEW_RUN_ID}",
                        }
                    ).encode(),
                )
            elif MODE_DISPATCH == "no-details-200":
                state["dispatched_at"] = time.time()
                self._send(200, b"{}")
            elif MODE_DISPATCH == "ok-204":
                state["dispatched_at"] = time.time()
                self._send(204)
            else:
                self._send(500, b'{"message":"stub boom"}')
        else:
            self._send(404, b'{"message":"not found"}')

    def do_GET(self):
        try:
            self._get()
        except Exception as exc:
            self._send(500, json.dumps({"message": f"stub error: {exc}"}).encode())

    def _get(self):
        m = re.match(rf"^/api/v1/repos/{re.escape(REPO)}/actions/runs/(?:(\d+)(?:/(.*))?)?", self.path)
        if m:
            rid = int(m.group(1)) if m.group(1) else None
            sub = m.group(2)
            if rid == NEW_RUN_ID:
                if sub == "jobs":
                    jobs = []
                    for name, jid in (("build", 8001), ("test", 8002), ("scan", 8003)):
                        job = {
                            "id": jid,
                            "run_id": NEW_RUN_ID,
                            "name": name,
                            "status": "queued",
                            "conclusion": None,
                            "runner_name": None,
                        }
                        if MODE_JOBS == "assigned" and name == "build":
                            job["runner_name"] = "stub-worker"
                        jobs.append(job)
                    self._send(200, json.dumps({"total_count": len(jobs), "jobs": jobs}).encode())
                    return
                state["run_polls"] += 1
                polls = state["run_polls"]
                if MODE_RUN == "success":
                    status, conclusion = "completed", "success"
                elif MODE_RUN == "failure":
                    if polls >= 2:
                        status, conclusion = "completed", "failure"
                    else:
                        status, conclusion = "in_progress", None
                elif MODE_RUN == "started":
                    status, conclusion = "in_progress", None
                else:  # stall
                    status, conclusion = "waiting", None
                self._send(200, json.dumps(run_obj(NEW_RUN_ID, status, conclusion, state["dispatched_at"] or time.time() - 60)).encode())
                return
            if rid == OLD_RUN_ID:
                self._send(
                    200,
                    json.dumps(run_obj(OLD_RUN_ID, "completed", "success", time.time() - 3600)).encode(),
                )
                return
            self._send(404, b'{"message":"run not found"}')
            return
        if self.path.startswith(f"/api/v1/repos/{REPO}/actions/runs"):
            created = state["dispatched_at"] or time.time() - 3600
            runs = [run_obj(OLD_RUN_ID, "completed", "success", time.time() - 3600)]
            if MODE_LIST in ("appears", "busy-pool") and state["dispatched_at"] is not None:
                if MODE_RUN == "stall":
                    runs.append(run_obj(NEW_RUN_ID, "waiting", None, created))
                elif MODE_RUN in ("failure", "started"):
                    runs.append(run_obj(NEW_RUN_ID, "in_progress", None, created))
                else:
                    runs.append(run_obj(NEW_RUN_ID, "completed", "success", created))
            if MODE_LIST == "busy-pool":
                runs.append(run_obj(BUSY_RUN_ID, "in_progress", None, time.time() - 60))
            self._send(200, json.dumps({"workflow_runs": runs, "total_count": len(runs)}).encode())
            return
        self._send(404, b'{"message":"not found"}')


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
