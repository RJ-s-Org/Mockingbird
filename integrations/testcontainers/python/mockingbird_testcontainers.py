"""A Testcontainers module for Mockingbird.

Starts the image with a contract (zero-config) or a mockingbird.yaml and its
contracts, waits for /readyz and exposes the mocks' base URL and the request log.
"""

from __future__ import annotations

import json
import os
import time
import urllib.error
import urllib.request
from pathlib import Path

from testcontainers.core.container import DockerContainer

DEFAULT_IMAGE = os.environ.get("MOCKINGBIRD_IMAGE", "ghcr.io/rj-s-org/mockingbird:0.1.0")
HTTP_PORT = 8080
ADMIN_PORT = 9090


class MockingbirdContainer(DockerContainer):
    def __init__(self, image: str = DEFAULT_IMAGE, **kwargs) -> None:
        super().__init__(image, **kwargs)
        self.with_exposed_ports(HTTP_PORT, ADMIN_PORT)
        self._serve: list[str] = []
        self._offline = True

    def with_contract(self, file: str | os.PathLike) -> "MockingbirdContainer":
        """Serves one contract zero-config: the mock is named after the file and mounted at `/`."""
        file = Path(file).resolve()
        self.with_volume_mapping(str(file.parent), "/contracts", "ro")
        self._serve = [f"/contracts/{file.name}"]
        return self

    def with_config_dir(self, directory: str | os.PathLike) -> "MockingbirdContainer":
        """Serves a directory holding mockingbird.yaml and its contracts; mocks are at `/<name>`."""
        self.with_volume_mapping(str(Path(directory).resolve()), "/etc/mockingbird", "ro")
        self._serve = ["-c", "/etc/mockingbird/mockingbird.yaml"]
        return self

    def with_llm(self, api_key: str | None = None, base_url: str | None = None, model: str | None = None):
        """Lets the mocks call an LLM. Pass the key from the environment, never a literal."""
        self._offline = False
        for name, value in (("MB_LLM_API_KEY", api_key), ("MB_LLM_BASE_URL", base_url), ("MB_LLM_MODEL", model)):
            if value:
                self.with_env(name, value)
        return self

    def start(self, timeout: float = 60) -> "MockingbirdContainer":
        args = ["serve", *self._serve, "--host", "0.0.0.0", "--insecure-no-auth"]
        if self._offline:
            args.append("--offline")
        self.with_command(args)
        super().start()
        self._wait_ready(timeout)
        return self

    def _wait_ready(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while True:
            try:
                with urllib.request.urlopen(f"{self.admin_url()}/readyz", timeout=2) as res:
                    if res.status == 200:
                        return
            except (urllib.error.URLError, ConnectionError, OSError):
                pass
            if time.monotonic() > deadline:
                stdout, stderr = self.get_logs()
                raise TimeoutError(f"Mockingbird not ready after {timeout}s:\n{stdout.decode()}{stderr.decode()}")
            time.sleep(0.5)

    def base_url(self, mock: str | None = None) -> str:
        """The data plane's URL, or a mock's (`/<mock>`) when the config names it."""
        url = f"http://{self.get_container_host_ip()}:{self.get_exposed_port(HTTP_PORT)}"
        return f"{url}/{mock}" if mock else url

    def admin_url(self) -> str:
        """The Admin API and ops endpoints (`/readyz`, `/api/v1/...`)."""
        return f"http://{self.get_container_host_ip()}:{self.get_exposed_port(ADMIN_PORT)}"

    def requests(self, mock: str) -> list[dict]:
        """The calls a mock received, newest first, with their bodies."""
        with urllib.request.urlopen(f"{self.admin_url()}/api/v1/mocks/{mock}/requests") as res:
            return json.load(res)
