import { useState, type FormEvent } from "react";
import { useActiveOperations, usePutScenario, type PutScenario, type Scenario } from "../../api/queries";
import { Drawer } from "../../components/Drawer";
import { ProblemAlert } from "../../components/ProblemAlert";
import { ChaosFields, chaosDraft, chaosErrors, chaosFromDraft, hasErrors } from "./ChaosLimits";
import { parseJson } from "../../lib/json";
import { Field, Notice, hintClass, inputClass, primaryButton } from "../../components/ui";

/** The `set` field's text parsed: an object, `null` when blank, or an error message. */
export function parseSet(text: string): { value: Record<string, unknown> | null } | { error: string } {
  if (text.trim() === "") return { value: null };
  const parsed = parseJson("Set", text);
  if ("error" in parsed) return parsed;
  const value = parsed.value;
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return { error: "Set must be a JSON object." };
  }
  return { value: value as Record<string, unknown> };
}

/** A number field's text: `null` when blank. */
function optionalNumber(text: string): number | null {
  return text.trim() === "" ? null : Number(text);
}

/** Creates or edits a scenario; read-only for file scenarios or without Edit. */
export function ScenarioEditor({
  ws,
  mock,
  scenario,
  canEdit,
  onClose,
}: {
  ws: string;
  mock: string;
  scenario?: Scenario;
  canEdit: boolean;
  onClose: () => void;
}) {
  const put = usePutScenario(ws, mock);
  const contractOperations = useActiveOperations(ws, mock);

  const [name, setName] = useState(scenario?.name ?? "");
  const [instructions, setInstructions] = useState(scenario?.instructions ?? "");
  const [status, setStatus] = useState(scenario?.status == null ? "" : String(scenario.status));
  const [latency, setLatency] = useState(scenario?.latency_ms == null ? "" : String(scenario.latency_ms));
  const [setText, setSetText] = useState(scenario?.set == null ? "" : JSON.stringify(scenario.set, null, 2));
  const [operations, setOperations] = useState<string[]>(scenario?.operations ?? []);
  const [chaos, setChaos] = useState(() => chaosDraft(scenario?.chaos));

  const reason =
    scenario?.source === "file"
      ? "Managed by config file"
      : canEdit
        ? undefined
        : "Read-only: editing a scenario needs the editor role.";
  const parsed = parseSet(setText);
  const setError = "error" in parsed ? parsed.error : undefined;
  const chaosErr = chaosErrors(chaos);
  const invalid = setError !== undefined || hasErrors(chaosErr);
  // Keep selections the active contract no longer has, so saving shows the server's verdict.
  const options = [...contractOperations, ...operations.filter((op) => !contractOperations.includes(op))];
  const title = scenario ? `Scenario ${scenario.name}` : "New scenario";

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if ("error" in parsed || invalid) return;
    const body: PutScenario = {
      instructions: instructions.trim() === "" ? null : instructions,
      status: optionalNumber(status),
      latency_ms: optionalNumber(latency),
      set: parsed.value as PutScenario["set"],
      operations,
    };
    // Not edited here: kept as they are.
    if (scenario?.headers) body.headers = scenario.headers;
    // An empty override is left out, which removes the scenario's chaos.
    const override = chaosFromDraft(chaos);
    if (override) body.chaos = override;
    put.mutate({ name, body }, { onSuccess: onClose });
  };

  return (
    <Drawer title={title} onClose={onClose}>
      <form onSubmit={submit} className="space-y-5">
        {reason ? <Notice tone="warn">{reason}</Notice> : null}
        <fieldset disabled={reason !== undefined} className="space-y-5">
          <legend className="sr-only">{title}</legend>
          {scenario ? null : (
            <Field
              id="scenario-name"
              label="Name"
              hint={
                <>
                  Letters, digits, dots, underscores and dashes; sent as <code>X-Mockingbird-Scenario</code>.
                </>
              }
            >
              <input
                id="scenario-name"
                className={inputClass}
                value={name}
                required
                maxLength={128}
                pattern="[A-Za-z0-9._\-]+"
                onChange={(e) => setName(e.target.value)}
                aria-describedby="scenario-name-hint"
              />
            </Field>
          )}
          <Field id="scenario-instructions" label="Instructions">
            <textarea
              id="scenario-instructions"
              className={inputClass}
              rows={4}
              value={instructions}
              onChange={(e) => setInstructions(e.target.value)}
            />
          </Field>
          <div className="grid gap-5 sm:grid-cols-2">
            <Field id="scenario-status" label="Status">
              <input
                id="scenario-status"
                type="number"
                min={100}
                max={599}
                className={inputClass}
                value={status}
                onChange={(e) => setStatus(e.target.value)}
              />
            </Field>
            <Field id="scenario-latency" label="Latency (ms)">
              <input
                id="scenario-latency"
                type="number"
                min={0}
                className={inputClass}
                value={latency}
                onChange={(e) => setLatency(e.target.value)}
              />
            </Field>
          </div>
          <Field
            id="scenario-set"
            label="Set (JSON object)"
            error={setError}
            hint={
              <>
                Fields forced into the response body, e.g. <code>{'{"status": "shipped"}'}</code>.
              </>
            }
          >
            <textarea
              id="scenario-set"
              className={`${inputClass} font-mono`}
              rows={5}
              value={setText}
              onChange={(e) => setSetText(e.target.value)}
              aria-invalid={setError !== undefined}
              aria-describedby={setError ? "scenario-set-error" : "scenario-set-hint"}
            />
          </Field>
          <Field
            id="scenario-operations"
            label="Operations"
            hint={
              contractOperations.length === 0
                ? "No active contract: the scenario applies to every operation."
                : "Operations of the active contract; none selected means all."
            }
          >
            <select
              id="scenario-operations"
              multiple
              size={Math.min(Math.max(options.length, 2), 8)}
              className={inputClass}
              value={operations}
              onChange={(e) => setOperations(Array.from(e.target.selectedOptions, (o) => o.value))}
              aria-describedby="scenario-operations-hint"
            >
              {options.map((op) => (
                <option key={op} value={op}>
                  {op}
                </option>
              ))}
            </select>
          </Field>
          <fieldset className="space-y-3 rounded-lg border border-line bg-surface-2/40 p-4">
            <legend className="px-1.5 text-sm font-semibold">Chaos override</legend>
            <p className={hintClass}>
              Replaces the mock&apos;s chaos while the scenario applies; leave empty to use the mock&apos;s.
            </p>
            <ChaosFields idPrefix="scenario-chaos" draft={chaos} errors={chaosErr} onChange={setChaos} />
          </fieldset>
        </fieldset>
        <ProblemAlert error={put.error} focus />
        {reason === undefined ? (
          <div className="flex flex-wrap items-center gap-3 border-t border-line pt-4">
            <button type="submit" className={primaryButton} disabled={invalid || put.isPending}>
              Save
            </button>
          </div>
        ) : null}
      </form>
    </Drawer>
  );
}
