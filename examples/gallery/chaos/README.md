# Chaos example

[`mockingbird.yaml`](mockingbird.yaml) shows per-mock chaos controls: injected errors
(`chaos.error_rate`), latency jitter (`chaos.latency_jitter`) and dropped streams
(`chaos.drop_stream_rate`), a per-mock rate limit (`limits.rate_limit`), and scenarios
whose chaos replaces the mock's (`outage` fails every call, `calm` turns errors off).

```sh
mockingbird serve -c examples/gallery/chaos/mockingbird.yaml --offline
```

Send `X-Mockingbird-Chaos: off` to bypass chaos for one request (honoured while the
mock's `header_overrides` is on, the default).
