import { useState, type FormEvent } from "react";
import { usePublish } from "../../api/channels";
import { Dialog, inputClass, primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";

/** The most messages one manual publish may generate. */
export const MAX_PUBLISH = 100;

/** The context field's text: an object, `undefined` when blank, or an error message. */
export function parseContext(text: string): { value: Record<string, unknown> | undefined } | { error: string } {
  if (text.trim() === "") return { value: undefined };
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (e) {
    return { error: `Context is not valid JSON: ${e instanceof Error ? e.message : String(e)}` };
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return { error: "Context must be a JSON object." };
  }
  return { value: value as Record<string, unknown> };
}

/** Where a published message came from: `llm`, `repaired` or `fallback`. */
function GenerationBadge({ source }: { source: string }) {
  const style =
    source === "fallback" ? "border-amber-300 bg-amber-50 text-amber-900" : "border-slate-300 bg-slate-50 text-slate-800";
  return (
    <span className={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium ${style}`}>
      {source}
    </span>
  );
}

/** Publishes messages on a channel now and shows what was published. */
export function PublishDialog({
  ws,
  mock,
  channel,
  onClose,
}: {
  ws: string;
  mock: string;
  channel: string;
  onClose: () => void;
}) {
  const publish = usePublish(ws, mock);
  const [count, setCount] = useState("1");
  const [contextText, setContextText] = useState("");

  const n = Number(count);
  const countError =
    count.trim() !== "" && Number.isInteger(n) && n >= 1 && n <= MAX_PUBLISH
      ? undefined
      : `Count must be between 1 and ${MAX_PUBLISH}.`;
  const context = parseContext(contextText);
  const contextError = "error" in context ? context.error : undefined;

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (countError || "error" in context) return;
    publish.mutate({ id: channel, count: n, ...(context.value ? { context: context.value } : {}) });
  };

  return (
    <Dialog title={`Publish on ${channel}`} onClose={onClose}>
      <form onSubmit={submit} className="space-y-4">
        <div className="space-y-1">
          <label htmlFor="publish-count" className="block text-sm font-medium">
            Count
          </label>
          <input
            id="publish-count"
            type="number"
            min={1}
            max={MAX_PUBLISH}
            className={inputClass}
            value={count}
            aria-invalid={countError !== undefined}
            aria-describedby={countError ? "publish-count-error" : undefined}
            onChange={(e) => setCount(e.target.value)}
          />
          {countError ? (
            <p id="publish-count-error" className="text-sm text-red-800">
              {countError}
            </p>
          ) : null}
        </div>
        <div className="space-y-1">
          <label htmlFor="publish-context" className="block text-sm font-medium">
            Context (JSON object)
          </label>
          <textarea
            id="publish-context"
            className={`${inputClass} font-mono`}
            rows={4}
            value={contextText}
            aria-invalid={contextError !== undefined}
            aria-describedby={contextError ? "publish-context-error" : "publish-context-hint"}
            onChange={(e) => setContextText(e.target.value)}
          />
          {contextError ? (
            <p id="publish-context-error" className="text-sm text-red-800">
              {contextError}
            </p>
          ) : (
            <p id="publish-context-hint" className="text-xs text-slate-600">
              Optional; handed to the message prompt.
            </p>
          )}
        </div>
        <ProblemAlert error={publish.error} />
        {publish.data ? (
          <div aria-live="polite" className="space-y-2">
            <h3 className="text-sm font-semibold">{`Published ${publish.data.messages.length} message(s)`}</h3>
            <ol className="max-h-80 space-y-3 overflow-y-auto">
              {publish.data.messages.map((m, i) => (
                <li key={i} className="space-y-1 rounded-md border border-slate-200 p-2 text-sm">
                  <div className="flex flex-wrap items-center gap-2">
                    <code className="break-all">{m.address}</code>
                    {m.key ? (
                      <span>
                        key <code>{m.key}</code>
                      </span>
                    ) : null}
                    <GenerationBadge source={m.source} />
                  </div>
                  <pre
                    aria-label="Published payload"
                    tabIndex={0}
                    className="overflow-x-auto rounded bg-slate-100 p-2 text-xs"
                  >
                    {JSON.stringify(m.payload, null, 2)}
                  </pre>
                </li>
              ))}
            </ol>
          </div>
        ) : null}
        <div className="flex justify-end gap-2">
          <button type="button" className={secondaryButton} onClick={onClose}>
            Close
          </button>
          <button
            type="submit"
            className={primaryButton}
            disabled={countError !== undefined || contextError !== undefined || publish.isPending}
          >
            Publish
          </button>
        </div>
      </form>
    </Dialog>
  );
}
