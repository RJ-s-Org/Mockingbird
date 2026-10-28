# OpenTelemetry OTLP (gRPC)

A stand-in for an OpenTelemetry Collector's OTLP/gRPC trace receiver, so you can test an
instrumented service or an exporter (retries, timeouts, partial success) without running
a collector. The contract is the real OTLP protos, so any OTLP/gRPC exporter talks to it
unchanged.

| | |
| --- | --- |
| Contract | [`otlp_trace_service.proto`](otlp_trace_service.proto): upstream `TraceService` and the trace, resource and common messages, merged into one file (same field numbers and method path) |
| Upstream | [open-telemetry/opentelemetry-proto](https://github.com/open-telemetry/opentelemetry-proto) (Apache-2.0), commit pinned in [`example.json`](example.json) and in the proto's header |
| Service | `opentelemetry.proto.collector.trace.v1.TraceService/Export` on the gRPC port (50051), with server reflection |
| Scenarios | `partial-success` (OK with `partial_success.rejected_spans`), `collector-unavailable` (`UNAVAILABLE`), `deadline-exceeded` (`DEADLINE_EXCEEDED` after 2 s) |
| Webhooks | none |

gRPC is served on its own plaintext port, not through the HTTPS proxy: point the exporter
at `http://127.0.0.1:50051` with `insecure=True`. No `Host` header, proxy variables or CA
are needed.

## Quickstart

```bash
mockingbird examples init grpc --dir grpc && cd grpc   # or: cd examples/apis/grpc
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
grpcurl -plaintext 127.0.0.1:50051 list
grpcurl -plaintext -d '{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},"scopeSpans":[{"spans":[{"traceId":"W47lZ5pQ0xGgrt0rYyLfSw==","spanId":"AAAAAAAAAAE=","name":"POST /cart","kind":"SPAN_KIND_SERVER"}]}]}]}' \
  127.0.0.1:50051 opentelemetry.proto.collector.trace.v1.TraceService/Export
```

The official OpenTelemetry Python exporter, unmodified (no proxy: gRPC goes straight to the port):

```bash
pip install opentelemetry-sdk opentelemetry-exporter-otlp-proto-grpc
python - <<'PY'
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter

spans = InMemorySpanExporter()
provider = TracerProvider(resource=Resource.create({"service.name": "checkout"}))
provider.add_span_processor(SimpleSpanProcessor(spans))
tracer = provider.get_tracer("juniper.checkout")
with tracer.start_as_current_span("POST /cart"):
    with tracer.start_as_current_span("SELECT inventory"):
        pass

exporter = OTLPSpanExporter(endpoint="http://127.0.0.1:50051", insecure=True, timeout=5)
print(len(spans.get_finished_spans()), "spans:", exporter.export(spans.get_finished_spans()))
PY
```

In a real app, use `BatchSpanProcessor(OTLPSpanExporter(...))` or set
`OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:50051 OTEL_EXPORTER_OTLP_INSECURE=true`.

## Expected output

The Python snippet prints `2 spans: SpanExportResult.SUCCESS`.

`--offline` answers come from the schema faker, which fills every field, so the grpcurl
call shows a `partialSuccess` with made-up values (a real collector that accepts every span
leaves it empty; the exporter ignores it either way):

```json
{
  "partialSuccess": {
    "rejectedSpans": "855486",
    "errorMessage": "pebble"
  }
}
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: an empty response for a healthy export.

## Scenarios

Send the scenario as gRPC metadata:

```bash
grpcurl -plaintext -H 'x-mockingbird-scenario: partial-success' -d '{}' \
  127.0.0.1:50051 opentelemetry.proto.collector.trace.v1.TraceService/Export
# {"partialSuccess": {"rejectedSpans": "2", "errorMessage": "2 spans dropped: trace_id must be 16 bytes"}}

grpcurl -plaintext -H 'x-mockingbird-scenario: collector-unavailable' -d '{}' \
  127.0.0.1:50051 opentelemetry.proto.collector.trace.v1.TraceService/Export
# ERROR: Code: Unavailable  Message: forced by x-mockingbird-status: 503
```

From the SDK, pass `headers={"x-mockingbird-scenario": "collector-unavailable"}` to
`OTLPSpanExporter`: it logs `Transient error StatusCode.UNAVAILABLE ... retrying`, backs off
until its timeout and returns `SpanExportResult.FAILURE`. Set `default_scenario` on the
mock to make every export fail that way.
