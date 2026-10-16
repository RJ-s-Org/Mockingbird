# Chaos example

This example will show per-mock chaos controls: injected errors (`chaos.error_rate`),
latency jitter (`chaos.latency_jitter`) and dropped streams (`chaos.drop_stream_rate`),
plus a scenario whose chaos replaces the mock's.

Its `mockingbird.yaml` is added together with the `chaos` configuration keys. Until
then this directory holds only this note.
