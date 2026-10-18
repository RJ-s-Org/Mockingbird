import { useState, type FormEvent } from "react";
import { usePatchMock, type ChaosSettings, type Mock, type MockLimits } from "../../api/queries";
import { inputClass, primaryButton } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";

/** Mirrors `ChaosSettings::MAX_JITTER_MS` in mb-store. */
const MAX_JITTER_MS = 30_000;
const MAX_REQUESTS_PER_SECOND = 100_000;
const MAX_BURST = 100_000;
const MAX_REQUEST_KIB = 67_108_864 / 1024;
const MAX_RESPONSE_KIB = 16_777_216 / 1024;

/** The chaos fields as typed: percentages and milliseconds. */
export interface ChaosDraft {
  errorPct: string;
  jitterMin: string;
  jitterMax: string;
  dropPct: string;
}

/** The limits fields as typed: requests/s, burst, tokens and KiB. */
export interface LimitsDraft {
  rps: string;
  burst: string;
  budget: string;
  requestKib: string;
  responseKib: string;
}

type Errors<T> = Partial<Record<keyof T, string>>;

/** A blank field is `undefined`; anything else is its number (maybe NaN). */
function parse(text: string): number | undefined {
  return text.trim() === "" ? undefined : Number(text);
}

/** A rate (0–1) as percent text; 0 and unset are blank. */
function percentText(rate: number | undefined): string {
  return rate ? String(Number((rate * 100).toFixed(6))) : "";
}

function text(n: number | null | undefined): string {
  return n == null ? "" : String(n);
}

/** The form text of stored chaos settings (`null` gives blank fields). */
export function chaosDraft(chaos: ChaosSettings | null | undefined): ChaosDraft {
  return {
    errorPct: percentText(chaos?.error_rate),
    jitterMin: text(chaos?.latency_jitter?.min_ms),
    jitterMax: text(chaos?.latency_jitter?.max_ms),
    dropPct: percentText(chaos?.drop_stream_rate),
  };
}

/** Client-side checks mirroring the store's ranges for chaos. */
export function chaosErrors(d: ChaosDraft): Errors<ChaosDraft> {
  const errors: Errors<ChaosDraft> = {};
  const pct = (v: string, what: string): string | undefined => {
    const n = parse(v);
    return n === undefined || (n >= 0 && n <= 100) ? undefined : `${what} must be between 0 and 100%.`;
  };
  const ms = (v: string, what: string): string | undefined => {
    const n = parse(v);
    return n === undefined || (Number.isInteger(n) && n >= 0 && n <= MAX_JITTER_MS)
      ? undefined
      : `${what} must be between 0 and ${MAX_JITTER_MS} ms.`;
  };
  errors.errorPct = pct(d.errorPct, "Error rate");
  errors.dropPct = pct(d.dropPct, "Dropped streams");
  errors.jitterMin = ms(d.jitterMin, "Jitter min");
  errors.jitterMax = ms(d.jitterMax, "Jitter max");
  if (!errors.jitterMin && !errors.jitterMax) {
    const min = parse(d.jitterMin) ?? 0;
    const max = parse(d.jitterMax);
    if (max === undefined && min > 0) errors.jitterMax = "Set a jitter max too.";
    else if (max !== undefined && min > max) errors.jitterMin = "Jitter min must not be above jitter max.";
  }
  return errors;
}

/** Valid chaos text as settings: `null` when every field is blank or 0. */
export function chaosFromDraft(d: ChaosDraft): ChaosSettings | null {
  const errorRate = (parse(d.errorPct) ?? 0) / 100;
  const dropRate = (parse(d.dropPct) ?? 0) / 100;
  const max = parse(d.jitterMax) ?? 0;
  const jitter = max > 0 ? { min_ms: parse(d.jitterMin) ?? 0, max_ms: max } : null;
  if (errorRate === 0 && dropRate === 0 && jitter === null) return null;
  return { error_rate: errorRate, drop_stream_rate: dropRate, latency_jitter: jitter };
}

function limitsDraft(limits: MockLimits | null | undefined): LimitsDraft {
  const kib = (bytes: number | null | undefined) => (bytes == null ? "" : String(bytes / 1024));
  return {
    rps: text(limits?.rate_limit?.requests_per_second),
    burst: text(limits?.rate_limit?.burst),
    budget: text(limits?.daily_token_budget),
    requestKib: kib(limits?.max_request_bytes),
    responseKib: kib(limits?.max_response_bytes),
  };
}

/** Client-side checks mirroring the store's ranges for limits. */
function limitsErrors(d: LimitsDraft): Errors<LimitsDraft> {
  const errors: Errors<LimitsDraft> = {};
  const rps = parse(d.rps);
  const burst = parse(d.burst);
  if (rps !== undefined && !(rps > 0 && rps <= MAX_REQUESTS_PER_SECOND)) {
    errors.rps = `Rate limit must be greater than 0 and at most ${MAX_REQUESTS_PER_SECOND} requests/s.`;
  }
  if (burst !== undefined && !(Number.isInteger(burst) && burst >= 1 && burst <= MAX_BURST)) {
    errors.burst = `Burst must be a whole number between 1 and ${MAX_BURST}.`;
  }
  if (rps === undefined && burst !== undefined) errors.rps = "Set both rate limit and burst, or neither.";
  if (burst === undefined && rps !== undefined) errors.burst = "Set both rate limit and burst, or neither.";
  const budget = parse(d.budget);
  if (budget !== undefined && !(Number.isSafeInteger(budget) && budget >= 1)) {
    errors.budget = "Daily token budget must be a whole number of at least 1.";
  }
  const size = (v: string, max: number, what: string): string | undefined => {
    const n = parse(v);
    if (n === undefined) return undefined;
    const bytes = Math.round(n * 1024);
    return bytes >= 1024 && bytes <= max * 1024 ? undefined : `${what} must be between 1 and ${max} KiB.`;
  };
  errors.requestKib = size(d.requestKib, MAX_REQUEST_KIB, "Max request size");
  errors.responseKib = size(d.responseKib, MAX_RESPONSE_KIB, "Max response size");
  return errors;
}

/** Valid limits text as the whole limits object; blank fields are `null`. */
function limitsFromDraft(d: LimitsDraft): MockLimits {
  const rps = parse(d.rps);
  const burst = parse(d.burst);
  const bytes = (v: string) => {
    const n = parse(v);
    return n === undefined ? null : Math.round(n * 1024);
  };
  return {
    rate_limit: rps !== undefined && burst !== undefined ? { requests_per_second: rps, burst } : null,
    daily_token_budget: parse(d.budget) ?? null,
    max_request_bytes: bytes(d.requestKib),
    max_response_bytes: bytes(d.responseKib),
  };
}

export function hasErrors<T>(errors: Errors<T>): boolean {
  return Object.values(errors).some((e) => e !== undefined);
}

/** A labelled number input with an optional error or hint below it. */
function NumberField({
  id,
  label,
  value,
  error,
  min,
  max,
  step,
  onChange,
}: {
  id: string;
  label: string;
  value: string;
  error?: string;
  min?: number;
  max?: number;
  step?: string;
  onChange: (value: string) => void;
}) {
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-sm font-medium">
        {label}
      </label>
      <input
        id={id}
        type="number"
        min={min}
        max={max}
        step={step}
        className={inputClass}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        aria-invalid={error !== undefined}
        aria-describedby={error ? `${id}-error` : undefined}
      />
      {error ? (
        <p id={`${id}-error`} className="text-sm text-red-800 dark:text-red-300">
          {error}
        </p>
      ) : null}
    </div>
  );
}

/** The three chaos settings: error rate, latency jitter and dropped streams. */
export function ChaosFields({
  idPrefix,
  draft,
  errors,
  onChange,
}: {
  idPrefix: string;
  draft: ChaosDraft;
  errors: Errors<ChaosDraft>;
  onChange: (draft: ChaosDraft) => void;
}) {
  const set = (key: keyof ChaosDraft) => (value: string) => onChange({ ...draft, [key]: value });
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      <NumberField
        id={`${idPrefix}-error-rate`}
        label="Error rate (%)"
        value={draft.errorPct}
        error={errors.errorPct}
        min={0}
        max={100}
        step="any"
        onChange={set("errorPct")}
      />
      <NumberField
        id={`${idPrefix}-drop-rate`}
        label="Dropped streams (%)"
        value={draft.dropPct}
        error={errors.dropPct}
        min={0}
        max={100}
        step="any"
        onChange={set("dropPct")}
      />
      <NumberField
        id={`${idPrefix}-jitter-min`}
        label="Latency jitter min (ms)"
        value={draft.jitterMin}
        error={errors.jitterMin}
        min={0}
        max={MAX_JITTER_MS}
        onChange={set("jitterMin")}
      />
      <NumberField
        id={`${idPrefix}-jitter-max`}
        label="Latency jitter max (ms)"
        value={draft.jitterMax}
        error={errors.jitterMax}
        min={0}
        max={MAX_JITTER_MS}
        onChange={set("jitterMax")}
      />
    </div>
  );
}

/** The Overview's "Chaos and limits" section; Save patches `chaos` and the whole `limits`. */
export function ChaosLimits({
  ws,
  mock,
  canEdit,
  reason,
  saved,
  onSaved,
}: {
  ws: string;
  mock: Mock;
  canEdit: boolean;
  /** Why the section is read-only, if it is. */
  reason: string | undefined;
  saved: boolean;
  onSaved: (updated: Mock) => void;
}) {
  const patch = usePatchMock(ws, mock.name);
  const [initialChaos] = useState(() => chaosDraft(mock.chaos));
  const [initialLimits] = useState(() => limitsDraft(mock.limits));
  const [chaos, setChaos] = useState(initialChaos);
  const [limits, setLimits] = useState(initialLimits);

  const chaosErr = chaosErrors(chaos);
  const limitsErr = limitsErrors(limits);
  const invalid = hasErrors(chaosErr) || hasErrors(limitsErr);
  const dirty =
    JSON.stringify(chaos) !== JSON.stringify(initialChaos) || JSON.stringify(limits) !== JSON.stringify(initialLimits);
  const setLimit = (key: keyof LimitsDraft) => (value: string) => setLimits({ ...limits, [key]: value });

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (invalid) return;
    patch.mutate({ chaos: chaosFromDraft(chaos), limits: limitsFromDraft(limits) }, { onSuccess: onSaved });
  };

  return (
    <section aria-labelledby="chaos-limits-heading" className="max-w-2xl space-y-4 border-t border-slate-200 dark:border-slate-800 pt-6">
      <h2 id="chaos-limits-heading" className="text-lg font-semibold">
        Chaos and limits
      </h2>
      <p className="text-xs text-slate-600 dark:text-slate-400">
        Callers can bypass chaos with X-Mockingbird-Chaos: off when header overrides are enabled.
      </p>
      {reason ? (
        <p className="rounded-md border border-amber-300 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/60 p-3 text-sm text-amber-900 dark:text-amber-200">{reason}</p>
      ) : null}
      {saved && !dirty ? (
        <p role="status" className="text-sm text-green-800 dark:text-green-300">
          Saved.
        </p>
      ) : null}
      <form onSubmit={submit} className="space-y-4">
        <fieldset disabled={reason !== undefined} className="space-y-4">
          <legend className="sr-only">Chaos and limits of {mock.name}</legend>
          <ChaosFields idPrefix="mock-chaos" draft={chaos} errors={chaosErr} onChange={setChaos} />
          <div className="grid gap-4 sm:grid-cols-2">
            <NumberField
              id="mock-rate-limit"
              label="Rate limit (requests/s)"
              value={limits.rps}
              error={limitsErr.rps}
              min={0}
              max={MAX_REQUESTS_PER_SECOND}
              step="any"
              onChange={setLimit("rps")}
            />
            <NumberField
              id="mock-burst"
              label="Burst"
              value={limits.burst}
              error={limitsErr.burst}
              min={1}
              max={MAX_BURST}
              onChange={setLimit("burst")}
            />
            <NumberField
              id="mock-token-budget"
              label="Daily token budget"
              value={limits.budget}
              error={limitsErr.budget}
              min={1}
              onChange={setLimit("budget")}
            />
            <div className="hidden sm:block" />
            <NumberField
              id="mock-max-request"
              label="Max request size (KiB)"
              value={limits.requestKib}
              error={limitsErr.requestKib}
              min={1}
              max={MAX_REQUEST_KIB}
              step="any"
              onChange={setLimit("requestKib")}
            />
            <NumberField
              id="mock-max-response"
              label="Max response size (KiB)"
              value={limits.responseKib}
              error={limitsErr.responseKib}
              min={1}
              max={MAX_RESPONSE_KIB}
              step="any"
              onChange={setLimit("responseKib")}
            />
          </div>
          <p className="text-xs text-slate-600 dark:text-slate-400">Blank limits fall back to the server&apos;s global limits.</p>
        </fieldset>
        <ProblemAlert error={patch.error} focus />
        {canEdit ? (
          <button
            type="submit"
            className={primaryButton}
            disabled={reason !== undefined || !dirty || invalid || patch.isPending}
          >
            Save chaos and limits
          </button>
        ) : null}
      </form>
    </section>
  );
}
