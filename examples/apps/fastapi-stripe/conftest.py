"""Starts Mockingbird (offline, with its proxy) and the app for the tests.

MOCKINGBIRD_BIN picks the binary (default: `mockingbird` on PATH). The app is
started with the variables `mockingbird proxy env` prints, exactly as a user
would run it, so stripe-python talks to https://api.stripe.com and is answered
by the mock.
"""

import json
import os
import subprocess
import tempfile
import threading
import time
from pathlib import Path

import httpx
import pytest
import uvicorn

HERE = Path(__file__).parent
MB = os.environ.get("MOCKINGBIRD_BIN", "mockingbird")
PORT, ADMIN, GRPC, PROXY = 48080, 48090, 48051, 48888
SECRET = "whsec_test_juniper"


def wait_for(url: str, timeout: float = 30) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if httpx.get(url, timeout=1).status_code == 200:
                return
        except httpx.HTTPError:
            pass
        time.sleep(0.2)
    raise RuntimeError(f"{url} did not answer within {timeout}s")


@pytest.fixture(scope="session")
def app_url():
    ca_dir = tempfile.mkdtemp(prefix="mb-ca-")
    # The server gets a clean environment: no proxy variables of its own.
    server_env = {k: v for k, v in os.environ.items() if "proxy" not in k.lower()}
    server_env.update(MB_CA_DIR=ca_dir, STRIPE_WEBHOOK_SECRET=SECRET)
    server = subprocess.Popen(
        [MB, "serve", "-c", str(HERE / "mockingbird.yaml"), "--offline",
         "--port", str(PORT), "--admin-port", str(ADMIN),
         "--grpc-port", str(GRPC), "--proxy-port", str(PROXY)],
        env=server_env,
    )
    try:
        wait_for(f"http://127.0.0.1:{ADMIN}/readyz")
        proxy_env = json.loads(subprocess.check_output(
            [MB, "proxy", "env", "--json", "--port", str(PROXY)],
            env={**server_env, "MB_CA_DIR": ca_dir},
        ))["vars"]
        os.environ.update(proxy_env)
        os.environ["STRIPE_WEBHOOK_SECRET"] = SECRET

        from app import app  # imported after the variables are set

        config = uvicorn.Config(app, host="127.0.0.1", port=4242, log_level="warning")
        web = uvicorn.Server(config)
        threading.Thread(target=web.run, daemon=True).start()
        wait_for("http://127.0.0.1:4242/events")
        yield "http://127.0.0.1:4242"
        web.should_exit = True
    finally:
        server.terminate()
        server.wait(timeout=10)
