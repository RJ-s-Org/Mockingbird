// A Testcontainers module for Mockingbird: starts the image with a contract
// (zero-config) or a mockingbird.yaml and its contracts, waits for /readyz and
// exposes the mocks' base URL and the request log.
import path from "node:path";
import { GenericContainer, Wait } from "testcontainers";

export const DEFAULT_IMAGE = process.env.MOCKINGBIRD_IMAGE ?? "ghcr.io/rj-s-org/mockingbird:0.1.0";
const HTTP_PORT = 8080;
const ADMIN_PORT = 9090;

export class MockingbirdContainer extends GenericContainer {
  #serve = [];
  #offline = true;

  constructor(image = DEFAULT_IMAGE) {
    super(image);
    this.withExposedPorts(HTTP_PORT, ADMIN_PORT).withWaitStrategy(
      Wait.forHttp("/readyz", ADMIN_PORT).forStatusCode(200),
    );
  }

  /** Serves one contract zero-config: the mock is named after the file and mounted at `/`. */
  withContract(file) {
    const target = `/contracts/${path.basename(file)}`;
    this.withCopyFilesToContainer([{ source: path.resolve(file), target }]);
    this.#serve = [target];
    return this;
  }

  /** Serves a directory holding mockingbird.yaml and the contracts it names; mocks are at `/<name>`. */
  withConfigDir(dir) {
    this.withCopyDirectoriesToContainer([{ source: path.resolve(dir), target: "/etc/mockingbird" }]);
    this.#serve = ["-c", "/etc/mockingbird/mockingbird.yaml"];
    return this;
  }

  /**
   * Lets the mocks call an LLM. Pass the key from the environment
   * (`process.env.MB_LLM_API_KEY`), never a literal.
   */
  withLlm({ apiKey, baseUrl, model } = {}) {
    this.#offline = false;
    this.withEnvironment({
      ...(apiKey && { MB_LLM_API_KEY: apiKey }),
      ...(baseUrl && { MB_LLM_BASE_URL: baseUrl }),
      ...(model && { MB_LLM_MODEL: model }),
    });
    return this;
  }

  async start() {
    this.withCommand([
      "serve",
      ...this.#serve,
      "--host",
      "0.0.0.0",
      "--insecure-no-auth",
      ...(this.#offline ? ["--offline"] : []),
    ]);
    return new StartedMockingbirdContainer(await super.start());
  }
}

export class StartedMockingbirdContainer {
  constructor(started) {
    this.container = started;
  }

  /** The data plane's URL, or a mock's (`/<mock>`) when the config names it. */
  baseUrl(mock) {
    const url = `http://${this.container.getHost()}:${this.container.getMappedPort(HTTP_PORT)}`;
    return mock ? `${url}/${mock}` : url;
  }

  /** The Admin API and ops endpoints (`/readyz`, `/api/v1/...`). */
  adminUrl() {
    return `http://${this.container.getHost()}:${this.container.getMappedPort(ADMIN_PORT)}`;
  }

  /** The calls a mock received, newest first, with their bodies. */
  async requests(mock) {
    const res = await fetch(`${this.adminUrl()}/api/v1/mocks/${mock}/requests`);
    if (!res.ok) throw new Error(`requests of ${mock}: ${res.status} ${await res.text()}`);
    return res.json();
  }

  stop(options) {
    return this.container.stop(options);
  }
}
