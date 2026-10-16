import { useState, type FormEvent } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useMock } from "../../api/queries";
import {
  IMPORT_FILE_EXTENSIONS,
  useCreateImport,
  type ImportKind,
  type ImportSource,
  type TextKind,
} from "../../api/imports";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { inputClass, primaryButton } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";
import { readOnlyReason } from "./Overview";

type SourceType = ImportSource["type"];

const SOURCES: { value: SourceType; label: string }[] = [
  { value: "file", label: "File" },
  { value: "url", label: "URL" },
  { value: "text", label: "Paste text" },
];

const TEXT_KINDS: { value: TextKind; label: string }[] = [
  { value: "markdown", label: "Markdown" },
  { value: "html", label: "HTML" },
  { value: "text", label: "Plain text" },
  { value: "har", label: "HAR recording" },
  { value: "postman", label: "Postman collection" },
];

const FILE_KINDS: { value: ImportKind; label: string }[] = [
  ...TEXT_KINDS.slice(0, 2),
  { value: "pdf", label: "PDF" },
  ...TEXT_KINDS.slice(2),
];

/** Starts an import from a file, a URL or pasted text; then shows its progress. */
export function ImportWizard({ ws, mock }: { ws: string; mock: string }) {
  const me = useMe();
  const m = useMock(ws, mock);
  const create = useCreateImport(ws, mock);
  const navigate = useNavigate();
  const [source, setSource] = useState<SourceType>("file");
  const [file, setFile] = useState<File>();
  const [fileKind, setFileKind] = useState<ImportKind | "">("");
  const [url, setUrl] = useState("");
  const [text, setText] = useState("");
  const [textKind, setTextKind] = useState<TextKind | "">("");

  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  const reason = m.data ? readOnlyReason(m.data, canEdit) : "";

  const chosen = (): ImportSource | undefined => {
    switch (source) {
      case "file":
        return file ? { type: "file", file, kind: fileKind || undefined } : undefined;
      case "url":
        return url.trim() ? { type: "url", url: url.trim() } : undefined;
      case "text":
        return text && textKind ? { type: "text", kind: textKind, text } : undefined;
    }
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const input = chosen();
    if (reason !== undefined || !input) return;
    create.mutate(input, {
      onSuccess: (job) =>
        void navigate({ to: "/w/$ws/mocks/$mock/import", params: { ws, mock }, search: { job: job.id } }),
    });
  };

  return (
    <form onSubmit={submit} className="max-w-2xl space-y-4 rounded-md border border-slate-300 bg-white p-4">
      <h2 className="text-lg font-semibold">Import API documentation</h2>
      <p className="text-sm text-slate-700">
        Turns Markdown, HTML or PDF documentation, a HAR recording or a Postman collection into a draft contract version
        to review before approval.
      </p>
      {reason ? (
        <p id="import-read-only" className="rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900">
          {reason}
        </p>
      ) : null}
      <fieldset className="space-y-1">
        <legend className="text-sm font-medium">Source</legend>
        <div className="flex flex-wrap gap-4">
          {SOURCES.map((s) => (
            <div key={s.value} className="flex items-center gap-2">
              <input
                id={`import-source-${s.value}`}
                type="radio"
                name="import-source"
                value={s.value}
                checked={source === s.value}
                onChange={() => setSource(s.value)}
              />
              <label htmlFor={`import-source-${s.value}`} className="text-sm">
                {s.label}
              </label>
            </div>
          ))}
        </div>
      </fieldset>

      {source === "file" ? (
        <div className="flex flex-wrap items-end gap-4">
          <div className="space-y-1">
            <label htmlFor="import-file" className="block text-sm font-medium">
              Document file
            </label>
            <input
              id="import-file"
              type="file"
              accept={IMPORT_FILE_EXTENSIONS.join(",")}
              className="text-sm"
              onChange={(e) => setFile(e.target.files?.[0])}
              aria-describedby="import-file-hint"
            />
            <p id="import-file-hint" className="text-xs text-slate-600">
              Markdown, HTML, PDF, plain text, a HAR recording or a Postman collection; at most 20 MiB.
            </p>
          </div>
          <div className="space-y-1">
            <label htmlFor="import-file-kind" className="block text-sm font-medium">
              Kind
            </label>
            <select
              id="import-file-kind"
              className={inputClass}
              value={fileKind}
              onChange={(e) => setFileKind(e.target.value as ImportKind | "")}
            >
              <option value="">Detect automatically</option>
              {FILE_KINDS.map((k) => (
                <option key={k.value} value={k.value}>
                  {k.label}
                </option>
              ))}
            </select>
          </div>
        </div>
      ) : null}

      {source === "url" ? (
        <div className="space-y-1">
          <label htmlFor="import-url" className="block text-sm font-medium">
            Document URL
          </label>
          <input
            id="import-url"
            type="url"
            required
            className={`${inputClass} w-full`}
            placeholder="https://docs.example.com/api"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
          />
        </div>
      ) : null}

      {source === "text" ? (
        <div className="space-y-3">
          <div className="space-y-1">
            <label htmlFor="import-text-kind" className="block text-sm font-medium">
              Kind
            </label>
            <select
              id="import-text-kind"
              required
              className={inputClass}
              value={textKind}
              onChange={(e) => setTextKind(e.target.value as TextKind | "")}
            >
              <option value="">Choose a kind</option>
              {TEXT_KINDS.map((k) => (
                <option key={k.value} value={k.value}>
                  {k.label}
                </option>
              ))}
            </select>
          </div>
          <div className="space-y-1">
            <label htmlFor="import-text" className="block text-sm font-medium">
              Document text
            </label>
            <textarea
              id="import-text"
              required
              rows={12}
              className={`${inputClass} w-full font-mono`}
              value={text}
              onChange={(e) => setText(e.target.value)}
            />
          </div>
        </div>
      ) : null}

      <button
        type="submit"
        className={primaryButton}
        disabled={reason !== undefined || create.isPending}
        title={reason || undefined}
        aria-describedby={reason ? "import-read-only" : undefined}
      >
        Start import
      </button>
      <ProblemAlert error={create.error} />
    </form>
  );
}
