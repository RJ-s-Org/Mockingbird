import { useRef, useState, type FormEvent } from "react";
import {
  fromMcmTrigger,
  toMcmTrigger,
  triggerError,
  useChannel,
  usePutChannel,
  type ChannelView,
  type StateChange,
  type TriggerDraft,
} from "../../api/channels";
import { inputClass, primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { Drawer } from "../../components/Drawer";
import { ProblemAlert } from "../../components/ProblemAlert";

const KINDS: { value: TriggerDraft["kind"]; label: string }[] = [
  { value: "schedule", label: "Schedule" },
  { value: "cron", label: "Cron" },
  { value: "on_state_change", label: "On state change" },
  { value: "manual", label: "Manual" },
];

const CHANGES: StateChange[] = ["any", "upserted", "deleted"];

/** A fresh draft of `kind`. */
function blank(kind: TriggerDraft["kind"]): TriggerDraft {
  switch (kind) {
    case "schedule":
      return { kind, every_ms: 1000 };
    case "cron":
      return { kind, expr: "" };
    case "on_state_change":
      return { kind, resource_type: "", change: "any" };
    case "manual":
      return { kind };
  }
}

interface Row {
  key: number;
  draft: TriggerDraft;
}

/** One trigger's fields. */
function TriggerFields({
  index,
  draft,
  onChange,
  onRemove,
}: {
  index: number;
  draft: TriggerDraft;
  onChange: (d: TriggerDraft) => void;
  onRemove: () => void;
}) {
  const n = index + 1;
  const id = (field: string) => `trigger-${index}-${field}`;
  const error = triggerError(draft);
  const errorId = id("error");
  const describedBy = error ? errorId : undefined;
  return (
    <fieldset className="space-y-3 rounded-md border border-slate-300 p-3">
      <legend className="px-1 text-sm font-semibold">{`Trigger ${n}`}</legend>
      <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1">
          <label htmlFor={id("kind")} className="block text-sm font-medium">
            Kind
          </label>
          <select
            id={id("kind")}
            className={inputClass}
            value={draft.kind}
            onChange={(e) => onChange(blank(e.target.value as TriggerDraft["kind"]))}
          >
            {KINDS.map((k) => (
              <option key={k.value} value={k.value}>
                {k.label}
              </option>
            ))}
          </select>
        </div>
        {draft.kind === "schedule" ? (
          <div className="space-y-1">
            <label htmlFor={id("every")} className="block text-sm font-medium">
              Every (ms)
            </label>
            <input
              id={id("every")}
              type="number"
              min={100}
              className={inputClass}
              value={Number.isFinite(draft.every_ms) ? draft.every_ms : ""}
              aria-invalid={error !== undefined}
              aria-describedby={describedBy}
              onChange={(e) =>
                onChange({ kind: "schedule", every_ms: e.target.value === "" ? NaN : Number(e.target.value) })
              }
            />
          </div>
        ) : null}
        {draft.kind === "cron" ? (
          <div className="min-w-48 flex-1 space-y-1">
            <label htmlFor={id("cron")} className="block text-sm font-medium">
              Cron expression
            </label>
            <input
              id={id("cron")}
              className={`${inputClass} font-mono`}
              value={draft.expr}
              placeholder="*/5 * * * *"
              aria-invalid={error !== undefined}
              aria-describedby={describedBy}
              onChange={(e) => onChange({ kind: "cron", expr: e.target.value })}
            />
          </div>
        ) : null}
        {draft.kind === "on_state_change" ? (
          <>
            <div className="space-y-1">
              <label htmlFor={id("resource")} className="block text-sm font-medium">
                Resource type
              </label>
              <input
                id={id("resource")}
                className={inputClass}
                value={draft.resource_type}
                aria-invalid={error !== undefined}
                aria-describedby={describedBy}
                onChange={(e) => onChange({ ...draft, resource_type: e.target.value })}
              />
            </div>
            <div className="space-y-1">
              <label htmlFor={id("change")} className="block text-sm font-medium">
                Change
              </label>
              <select
                id={id("change")}
                className={inputClass}
                value={draft.change}
                onChange={(e) => onChange({ ...draft, change: e.target.value as StateChange })}
              >
                {CHANGES.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
            </div>
          </>
        ) : null}
        <button type="button" className={secondaryButton} aria-label={`Remove trigger ${n}`} onClick={onRemove}>
          Remove
        </button>
      </div>
      {error ? (
        <p id={errorId} className="text-sm text-red-800">
          {error}
        </p>
      ) : null}
    </fieldset>
  );
}

/** The editor once the channel is loaded. */
function TriggersForm({
  ws,
  mock,
  channel,
  onClose,
}: {
  ws: string;
  mock: string;
  channel: ChannelView;
  onClose: () => void;
}) {
  const put = usePutChannel(ws, mock);
  const parsed = channel.triggers.map(fromMcmTrigger);
  const readable = parsed.filter((d): d is TriggerDraft => d !== null);
  const unreadable = parsed.length - readable.length;
  const [useContract, setUseContract] = useState(channel.settings_source === "contract");
  const [rows, setRows] = useState<Row[]>(() => readable.map((draft, key) => ({ key, draft })));
  // Row keys after the initial ones; only read in event handlers.
  const nextKey = useRef(readable.length);
  const addRow = () => {
    const key = nextKey.current;
    nextKey.current += 1;
    setRows((rs) => [...rs, { key, draft: blank("manual") }]);
  };

  const invalid = !useContract && rows.some((r) => triggerError(r.draft) !== undefined);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (invalid) return;
    put.mutate(
      {
        id: channel.id,
        enabled: channel.enabled,
        triggers: useContract ? null : rows.map((r) => toMcmTrigger(r.draft)),
      },
      { onSuccess: onClose },
    );
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      <label className="flex items-center gap-2 text-sm font-medium">
        <input type="checkbox" checked={useContract} onChange={(e) => setUseContract(e.target.checked)} />
        Use the contract&apos;s triggers
      </label>
      {useContract ? (
        <p className="text-sm text-slate-700">The channel publishes on the triggers its contract declares.</p>
      ) : (
        <div className="space-y-3">
          {unreadable > 0 ? (
            <p className="rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900">
              {`${unreadable} trigger(s) this editor cannot show will be dropped on save.`}
            </p>
          ) : null}
          {rows.length === 0 ? (
            <p className="text-sm text-slate-700">No triggers: the channel publishes only on demand.</p>
          ) : null}
          {rows.map((row, i) => (
            <TriggerFields
              key={row.key}
              index={i}
              draft={row.draft}
              onChange={(draft) => setRows((rs) => rs.map((r) => (r.key === row.key ? { ...r, draft } : r)))}
              onRemove={() => setRows((rs) => rs.filter((r) => r.key !== row.key))}
            />
          ))}
          <button
            type="button"
            className={secondaryButton}
            onClick={addRow}
          >
            Add trigger
          </button>
        </div>
      )}
      <ProblemAlert error={put.error} />
      <button type="submit" className={primaryButton} disabled={invalid || put.isPending}>
        Save
      </button>
    </form>
  );
}

/** A drawer editing a publish channel's triggers. */
export function ChannelTriggersEditor({
  ws,
  mock,
  channel,
  onClose,
}: {
  ws: string;
  mock: string;
  channel: ChannelView;
  onClose: () => void;
}) {
  const detail = useChannel(ws, mock, channel.id);
  return (
    <Drawer title={`Triggers of ${channel.id}`} onClose={onClose}>
      {detail.data ? (
        <TriggersForm ws={ws} mock={mock} channel={detail.data} onClose={onClose} />
      ) : detail.error ? (
        <ProblemAlert error={detail.error} />
      ) : (
        <p className="text-slate-700">Loading…</p>
      )}
    </Drawer>
  );
}
