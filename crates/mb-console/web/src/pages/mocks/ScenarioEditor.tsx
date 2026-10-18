import { useState, type FormEvent } from "react";
import { useActiveOperations, usePutScenario, type PutScenario, type Scenario } from "../../api/queries";
import { inputClass, primaryButton } from "../../components/ConfirmDialog";
import { Drawer } from "../../components/Drawer";
import { ProblemAlert } from "../../components/ProblemAlert";
import { ChaosFields, chaosDraft, chaosErrors, chaosFromDraft, hasErrors } from "./ChaosLimits";
import { parseJson } from "../../lib/json";

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
    // An empty override is left out, which removes the scenario's chaos.
    const override = chaosFromDraft(chaos);
    if (override) body.chaos = override;
    put.mutate({ name, body }, { onSuccess: onClose });
  };

  return (
    <Drawer title={title} onClose={onClose}>
      <form onSubmit={submit} className="space-y-4">
        {reason ? (
          <p className="rounded-md border border-amber-300 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/60 p-3 text-sm text-amber-900 dark:text-amber-200">{reason}</p>
        ) : null}
        <fieldset disabled={reason !== undefined} className="space-y-4">
          <legend className="sr-only">{title}</legend>
          {scenario ? null : (
            <div className="space-y-1">
              <label htmlFor="scenario-name" className="block text-sm font-medium">
                Name
              </label>
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
              <p id="scenario-name-hint" className="text-xs text-slate-600 dark:text-slate-400">
                Letters, digits, dots, underscores and dashes; sent as <code>X-Mockingbird-Scenario</code>.
              </p>
            </div>
          )}
          <div className="space-y-1">
            <label htmlFor="scenario-instructions" className="block text-sm font-medium">
              Instructions
            </label>
            <textarea
              id="scenario-instructions"
              className={inputClass}
              rows={4}
              value={instructions}
              onChange={(e) => setInstructions(e.target.value)}
            />
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <div className="space-y-1">
              <label htmlFor="scenario-status" className="block text-sm font-medium">
                Status
              </label>
              <input
                id="scenario-status"
                type="number"
                min={100}
                max={599}
                className={inputClass}
                value={status}
                onChange={(e) => setStatus(e.target.value)}
              />
            </div>
            <div className="space-y-1">
              <label htmlFor="scenario-latency" className="block text-sm font-medium">
                Latency (ms)
              </label>
              <input
                id="scenario-latency"
                type="number"
                min={0}
                className={inputClass}
                value={latency}
                onChange={(e) => setLatency(e.target.value)}
              />
            </div>
          </div>
          <div className="space-y-1">
            <label htmlFor="scenario-set" className="block text-sm font-medium">
              Set (JSON object)
            </label>
            <textarea
              id="scenario-set"
              className={`${inputClass} font-mono`}
              rows={5}
              value={setText}
              onChange={(e) => setSetText(e.target.value)}
              aria-invalid={setError !== undefined}
              aria-describedby={setError ? "scenario-set-error" : "scenario-set-hint"}
            />
            {setError ? (
              <p id="scenario-set-error" className="text-sm text-red-800 dark:text-red-300">
                {setError}
              </p>
            ) : (
              <p id="scenario-set-hint" className="text-xs text-slate-600 dark:text-slate-400">
                Fields forced into the response body, e.g. <code>{'{"status": "shipped"}'}</code>.
              </p>
            )}
          </div>
          <div className="space-y-1">
            <label htmlFor="scenario-operations" className="block text-sm font-medium">
              Operations
            </label>
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
            <p id="scenario-operations-hint" className="text-xs text-slate-600 dark:text-slate-400">
              {contractOperations.length === 0
                ? "No active contract: the scenario applies to every operation."
                : "Operations of the active contract; none selected means all."}
            </p>
          </div>
          <fieldset className="space-y-2 rounded-md border border-slate-200 dark:border-slate-800 p-3">
            <legend className="px-1 text-sm font-medium">Chaos override</legend>
            <p className="text-xs text-slate-600 dark:text-slate-400">
              Replaces the mock&apos;s chaos while the scenario applies; leave empty to use the mock&apos;s.
            </p>
            <ChaosFields idPrefix="scenario-chaos" draft={chaos} errors={chaosErr} onChange={setChaos} />
          </fieldset>
        </fieldset>
        <ProblemAlert error={put.error} />
        {reason === undefined ? (
          <button type="submit" className={primaryButton} disabled={invalid || put.isPending}>
            Save
          </button>
        ) : null}
      </form>
    </Drawer>
  );
}
