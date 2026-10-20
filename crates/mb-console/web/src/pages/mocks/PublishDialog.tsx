import { useState, type FormEvent } from "react";
import { usePublish } from "../../api/channels";
import { Dialog } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";
import { parseJson } from "../../lib/json";
import { ResponseSource } from "../../components/http";
import { Field, inputClass, preClass, primaryButton, secondaryButton } from "../../components/ui";

/** The most messages one manual publish may generate. */
export const MAX_PUBLISH = 100;

/** The context field's text: an object, `undefined` when blank, or an error message. */
export function parseContext(text: string): { value: Record<string, unknown> | undefined } | { error: string } {
  if (text.trim() === "") return { value: undefined };
  const parsed = parseJson("Context", text);
  if ("error" in parsed) return parsed;
  const value = parsed.value;
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return { error: "Context must be a JSON object." };
  }
  return { value: value as Record<string, unknown> };
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
    <Dialog title={`Publish on ${channel}`} onClose={onClose} wide>
      <form onSubmit={submit} className="space-y-5">
        <Field id="publish-count" label="Count" error={countError} className="sm:max-w-40">
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
        </Field>
        <Field
          id="publish-context"
          label="Context (JSON object)"
          error={contextError}
          hint="Optional; handed to the message prompt."
        >
          <textarea
            id="publish-context"
            className={`${inputClass} font-mono`}
            rows={4}
            value={contextText}
            aria-invalid={contextError !== undefined}
            aria-describedby={contextError ? "publish-context-error" : "publish-context-hint"}
            onChange={(e) => setContextText(e.target.value)}
          />
        </Field>
        <ProblemAlert error={publish.error} focus />
        {publish.data ? (
          <div aria-live="polite" className="space-y-2">
            <h3 className="text-sm font-semibold">{`Published ${publish.data.messages.length} message(s)`}</h3>
            <ol className="max-h-80 space-y-3 overflow-y-auto">
              {publish.data.messages.map((m, i) => (
                <li key={i} className="space-y-2 rounded-md border border-line bg-surface p-3 text-sm">
                  <div className="flex flex-wrap items-center gap-2">
                    <code className="font-medium break-all">{m.address}</code>
                    {m.key ? (
                      <span className="text-fg-muted">
                        key <code>{m.key}</code>
                      </span>
                    ) : null}
                    <ResponseSource source={m.source} />
                  </div>
                  <pre
                    aria-label="Published payload"
                    tabIndex={0}
                    className={preClass}
                  >
                    {JSON.stringify(m.payload, null, 2)}
                  </pre>
                </li>
              ))}
            </ol>
          </div>
        ) : null}
        <div className="flex justify-end gap-2 pt-2">
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
