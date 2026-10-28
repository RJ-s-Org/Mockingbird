package io.mockingbird.testcontainers;

import java.io.IOException;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import org.testcontainers.containers.GenericContainer;
import org.testcontainers.containers.wait.strategy.Wait;
import org.testcontainers.utility.DockerImageName;
import org.testcontainers.utility.MountableFile;

/**
 * A Testcontainers module for Mockingbird: starts the image with a contract (zero-config) or a
 * mockingbird.yaml and its contracts, waits for /readyz and exposes the mocks' base URL and the
 * request log.
 */
public class MockingbirdContainer extends GenericContainer<MockingbirdContainer> {
  public static final String DEFAULT_IMAGE =
      System.getenv().getOrDefault("MOCKINGBIRD_IMAGE", "ghcr.io/rj-s-org/mockingbird:0.1.0");
  private static final int HTTP_PORT = 8080;
  private static final int ADMIN_PORT = 9090;

  private final List<String> serve = new ArrayList<>();
  private boolean offline = true;

  public MockingbirdContainer() {
    this(DockerImageName.parse(DEFAULT_IMAGE));
  }

  public MockingbirdContainer(DockerImageName image) {
    super(image);
    withExposedPorts(HTTP_PORT, ADMIN_PORT);
    waitingFor(Wait.forHttp("/readyz").forPort(ADMIN_PORT).forStatusCode(200));
  }

  /** Serves one contract zero-config: the mock is named after the file and mounted at `/`. */
  public MockingbirdContainer withContract(Path file) {
    String target = "/contracts/" + file.getFileName();
    withCopyFileToContainer(MountableFile.forHostPath(file), target);
    serve.clear();
    serve.add(target);
    return this;
  }

  /** Serves a directory holding mockingbird.yaml and its contracts; mocks are at `/<name>`. */
  public MockingbirdContainer withConfigDir(Path dir) {
    withCopyFileToContainer(MountableFile.forHostPath(dir), "/etc/mockingbird");
    serve.clear();
    serve.addAll(List.of("-c", "/etc/mockingbird/mockingbird.yaml"));
    return this;
  }

  /** Lets the mocks call an LLM. Pass the key from the environment, never a literal. */
  public MockingbirdContainer withLlm(String apiKey, String baseUrl, String model) {
    offline = false;
    if (apiKey != null) withEnv("MB_LLM_API_KEY", apiKey);
    if (baseUrl != null) withEnv("MB_LLM_BASE_URL", baseUrl);
    if (model != null) withEnv("MB_LLM_MODEL", model);
    return this;
  }

  @Override
  protected void configure() {
    List<String> cmd = new ArrayList<>(List.of("serve"));
    cmd.addAll(serve);
    cmd.addAll(List.of("--host", "0.0.0.0", "--insecure-no-auth"));
    if (offline) cmd.add("--offline");
    setCommand(cmd.toArray(String[]::new));
  }

  /** The data plane's URL. */
  public String getBaseUrl() {
    return "http://" + getHost() + ":" + getMappedPort(HTTP_PORT);
  }

  /** A mock's URL (`/<mock>`), for mocks the config names. */
  public String getBaseUrl(String mock) {
    return getBaseUrl() + "/" + mock;
  }

  /** The Admin API and ops endpoints (`/readyz`, `/api/v1/...`). */
  public String getAdminUrl() {
    return "http://" + getHost() + ":" + getMappedPort(ADMIN_PORT);
  }

  /** The calls a mock received, newest first, with their bodies, as JSON. */
  public String requests(String mock) throws IOException, InterruptedException {
    HttpResponse<String> res =
        HttpClient.newHttpClient()
            .send(
                HttpRequest.newBuilder(
                        URI.create(getAdminUrl() + "/api/v1/mocks/" + mock + "/requests"))
                    .build(),
                HttpResponse.BodyHandlers.ofString());
    if (res.statusCode() != 200) {
      throw new IOException("requests of " + mock + ": " + res.statusCode() + " " + res.body());
    }
    return res.body();
  }
}
