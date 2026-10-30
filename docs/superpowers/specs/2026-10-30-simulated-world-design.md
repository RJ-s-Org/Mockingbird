# Simulated worlds at runtime: design

Status: draft for review. Date: 2026-10-30. Evidence: round 2 benchmark data in `docs/benchmarks/2026-10-30/`.

## Goal

A mock should behave like a small simulated copy of the API: it answers requests nobody anticipated, remembers what it said, and shows the consequences of writes. Teams get this from a contract and, at most, a one-line persona.

The LLM should spend its effort on judgement (what is plausible, what a request means, what a write causes). The engine should own what a machine does exactly (structure, opaque IDs, constraints, bookkeeping).

## Constraints

1. **No new required configuration.** Setup stays a contract plus an optional persona. Nothing in this design needs authoring, approval or maintenance. Scenarios, fixtures and `--offline` keep working as they do.
2. **The LLM stays on the request path** for anything new. There is no pre-built dataset and no canned answer store.
3. **Every answer still matches the contract**, on every protocol.
4. **Secrets stay references.** No API key enters a prompt, a stored entity, a log or a report.

## Rejected

- **A blueprint or pre-built world as a reviewed artifact.** Even when the LLM writes it, a team has to review and maintain it, and a frozen dataset cannot answer the unanticipated.
- **Constrained decoding as the core approach.** Provider support varies, and it fixes neither placeholder values nor wrong sums.

## Evidence

From 1,944 round 2 engine calls:

| Share of generated answer bytes | |
|---|---:|
| Keys, brackets, nulls | 49% |
| Values under a pattern, format or const | 28% |
| Values with ID-like names | 3% |
| Enums and booleans | 5% |
| Numbers | 2% |
| Free text | 14% |

- First try is 59.7% per call. It is 100% on gRPC for every model and 15–76% on REST.
- 30 of 84 judged samples scored below 4 on realism or coherence. A rough keyword count finds 23 mentioning placeholder values and 16 mentioning wrong sums or mismatches.
- 45% of successful answers hold a nested object with its own `id`, and 1,051 of 1,885 hold a foreign-ID field.
- Calls average 549 completion tokens and 1,108 prompt tokens.

These are byte counts and keyword matches over bench traffic, not token counts or production traffic. The pilot below tests the conclusions drawn from them.

## Design

### 1. Values-only generation

The engine derives a **slot plan** from the operation's response schema when the bundle is compiled. Each leaf is one of:

| Slot | Example | Who fills it |
|---|---|---|
| Structural | keys, `const`, array shells | Engine |
| Opaque | an `id`, token or secret, or any string under a hard `pattern` | Engine, from the pattern or a prefix learned from the contract |
| Meaningful | names, descriptions, emails, dates, amounts, statuses | LLM; the engine validates |
| Choice | `enum`, boolean, `oneOf` branch | LLM picks from the listed options |

The classification reads only the contract. Dates and emails stay with the LLM because they carry meaning.

The LLM receives the meaningful and choice slots with their paths and returns a flat sheet. The engine assembles the document, so a structural error cannot happen. A slot the LLM omits or fills invalidly is filled by the faker, and the response reports it. Array lengths and `oneOf` branches are chosen by the LLM first, within the schema's limits, because they decide which slots exist.

The sheet schema is flat and free of patterns, which should suit `output_mode: json_schema` far better than full contract schemas.

Full-document generation stays as the path for operations the slot plan cannot express, and for scenarios that need it. The source header gains a value for assembled answers.

### 2. A world that grows from traffic

Whatever the LLM invents is remembered:

- After each answer, the engine walks the body and stores nested objects with an identity as entities of their resource type, in the session's existing state store.
- A foreign-ID field (`customer_id`) is recorded as a reference. A later read of that ID answers from the stored entity, or, when only the reference exists, generates the entity once, told what refers to it.
- The prompt's `related` context comes from following references from the request's target, under a fixed token limit.

Sessions stay isolated. A shared base world across sessions is out of scope until a need appears.

### 3. Writes have consequences

A write's generation may also return **effects**: updates to other entities it names by type and ID.

```json
{"response": {"status": "succeeded"},
 "effects": [{"type": "charges", "id": "ch_1", "set": {"amount_refunded": 4800}}]}
```

The engine applies an effect only when the entity exists and the merged entity still validates against its schema. Rejected effects are dropped and counted. Applied effects publish state changes, so the response cache, change feeds, triggers and webhooks see them. Effects cost no extra LLM call.

### 4. Learned invariants (last, optional)

From the contract and early answers, a background call proposes invariants such as `durationMinutes = arrival − departure`, in a small expression language with no code execution. The engine then computes those fields. Rules are a disposable cache, can be inspected, and can be corrected by a sentence in the persona. Nobody has to approve them.

This slice ships only if the pilot shows that arithmetic errors still hurt coherence after slices 1–3.

## Protocols

The slot plan works on the engine's JSON Schema, so GraphQL, gRPC, SOAP and AsyncAPI inherit it. Streams and event messages generate one item per sheet. The first implementation may keep full-document generation for streams.

## Measurement

SV and First try approach 100% under this design, as they already do for the faker, so they stop being the success measure. The bench adds:

- **Journeys:** multi-step flows across related resources (create → act → read back), graded for whether later answers reflect earlier ones.
- **Request fidelity:** whether an answer reflects the request's filters, IDs and body.
- **Slot health:** the rate of LLM-omitted or invalid slots, and rejected effects.

Blind-judged realism and coherence, cost and latency stay as they are.

## Pilot, before any engine work on slices 1–4

A prototype in `scripts/llm-bench`, using the round 2 contracts and no added configuration: about 6 contracts and 3 models, including the default.

| Test | Pass |
|---|---|
| Single calls | Realism and coherence at least match round 2 for the same models and operations, with lower cost and p50 latency |
| Journeys | Later answers reflect earlier ones more often than today's engine on the same journeys |

If single calls fail, values-only generation is dropped. Estimated spend is under $1, from the $2.73 left under the $9.50 cap.

## Risks

| Risk | Mitigation |
|---|---|
| Separate slot values lose coherence between fields | The sheet is one LLM answer with all slots visible; the pilot measures it |
| Opaque values generated by the engine contradict the request | Values the request sent are always kept |
| Effects accumulate facts that are valid but wrong | Effects are visible in the request log; a session reset clears them |
| Prompts grow with world context | A fixed token limit, and references are followed nearest first |
| Wrong learned invariants are consistently wrong | Slice 4 comes last, is optional and can be inspected |
| A reproducible run is harder | `--offline` and fixtures stay the route for that |

## Delivery order

1. Pilot and its results, for a decision.
2. Slot plan and values-only generation, including engine-owned opaque values.
3. Entity extraction and references.
4. Effects.
5. Learned invariants, if the pilot's evidence calls for them.

Each is one PR with the standard gates. The separate near-term fixes (first-try reporting, patching inside `oneOf`/`anyOf`, coherent patches, the echo exemption for `pattern`) do not depend on this design and stay useful for full-document generation.
