package io.mockingbird.testcontainers;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;
import org.testcontainers.junit.jupiter.Container;
import org.testcontainers.junit.jupiter.Testcontainers;

@Testcontainers
class MockingbirdContainerTest {
  @Container
  static final MockingbirdContainer MOCKS =
      new MockingbirdContainer().withConfigDir(Path.of("../fixtures").toAbsolutePath().normalize());

  private static HttpResponse<String> charge(String scenario) throws Exception {
    HttpRequest.Builder req =
        HttpRequest.newBuilder(URI.create(MOCKS.getBaseUrl("payments") + "/v1/charges"))
            .header("Content-Type", "application/json")
            .POST(HttpRequest.BodyPublishers.ofString("{\"amount\":1999,\"currency\":\"eur\"}"));
    if (scenario != null) req.header("X-Mockingbird-Scenario", scenario);
    return HttpClient.newHttpClient().send(req.build(), HttpResponse.BodyHandlers.ofString());
  }

  @Test
  void aChargeIsCreatedAndRecorded() throws Exception {
    HttpResponse<String> res = charge(null);
    assertEquals(201, res.statusCode());
    assertTrue(res.body().contains("\"id\""));
    assertTrue(MOCKS.requests("payments").contains("1999"));
  }

  @Test
  void aScenarioDeclinesTheCard() throws Exception {
    assertEquals(402, charge("card-declined").statusCode());
  }
}
