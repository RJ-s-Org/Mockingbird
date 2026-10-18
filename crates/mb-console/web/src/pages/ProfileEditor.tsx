import { useState, type FormEvent, type InputHTMLAttributes } from "react";
import { usePutProfile, type Profile, type ProfileWriteResult } from "../api/queries";
import { inputClass, primaryButton, secondaryButton } from "../components/ConfirmDialog";
import { Drawer } from "../components/Drawer";
import { ProblemAlert } from "../components/ProblemAlert";

/**
 * What the server's default `server.api_secret_refs` policy accepts from the
 * API (the schema's `env:NAME or file:/path` is the syntax, not the policy).
 */
export const SECRET_REF_HINT = "Use env:MB_LLM_<NAME> or env:<NAME>_API_KEY. The key itself is never stored.";

/** A labelled text or number field of the profile form. */
function Field({
  id,
  label,
  hint,
  ...input
}: { id: string; label: string; hint?: string } & InputHTMLAttributes<HTMLInputElement>) {
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-sm font-medium">
        {label}
      </label>
      <input id={id} className={inputClass} aria-describedby={hint ? `${id}-hint` : undefined} {...input} />
      {hint ? (
        <p id={`${id}-hint`} className="text-xs text-slate-600 dark:text-slate-400">
          {hint}
        </p>
      ) : null}
    </div>
  );
}

/**
 * Creates (`profile` absent) or replaces an LLM profile. The key itself is
 * never entered here: only a `secret_ref` naming where the server reads it.
 */
export function ProfileEditor({
  ws,
  profile,
  profiles,
  onClose,
  onSaved,
}: {
  ws: string;
  profile?: Profile;
  /** Every profile of the workspace, for the fallback choice. */
  profiles: Profile[];
  onClose: () => void;
  onSaved: (result: ProfileWriteResult) => void;
}) {
  const put = usePutProfile(ws);
  const [name, setName] = useState(profile?.name ?? "");
  const [baseUrl, setBaseUrl] = useState(profile?.base_url ?? "");
  const [model, setModel] = useState(profile?.model ?? "");
  const [secretRef, setSecretRef] = useState(profile?.secret_ref ?? "");
  const [jsonSchema, setJsonSchema] = useState(profile?.json_schema ?? false);
  const [temperature, setTemperature] = useState(String(profile?.temperature ?? 0.7));
  const [timeoutMs, setTimeoutMs] = useState(String(profile?.timeout_ms ?? 3000));
  const [maxTokens, setMaxTokens] = useState(String(profile?.max_tokens ?? 2048));
  const [fallback, setFallback] = useState(profile?.fallback_profile ?? "");

  const fallbacks = profiles.map((p) => p.name).filter((n) => n !== (profile?.name ?? name));
  if (fallback && !fallbacks.includes(fallback)) fallbacks.unshift(fallback);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    put.mutate(
      {
        name,
        body: {
          base_url: baseUrl.trim(),
          model: model.trim(),
          secret_ref: secretRef.trim() || null,
          json_schema: jsonSchema,
          temperature: Number(temperature),
          timeout_ms: Number(timeoutMs),
          max_tokens: Number(maxTokens),
          fallback_profile: fallback || null,
        },
      },
      { onSuccess: onSaved },
    );
  };

  return (
    <Drawer title={profile ? `Edit profile ${profile.name}` : "New profile"} onClose={onClose}>
      <form onSubmit={submit} className="space-y-4">
        {profile ? null : (
          <Field
            id="profile-name"
            label="Name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
            pattern="[a-z0-9][a-z0-9\-]{0,62}"
            hint="Lowercase letters, digits and dashes."
          />
        )}
        <Field
          id="profile-base-url"
          label="Base URL"
          type="url"
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          required
          hint="An OpenAI-compatible endpoint, e.g. https://api.openai.com/v1"
        />
        <Field id="profile-model" label="Model" value={model} onChange={(e) => setModel(e.target.value)} required />
        <Field
          id="profile-secret-ref"
          label="Secret ref"
          value={secretRef}
          onChange={(e) => setSecretRef(e.target.value)}
          placeholder="env:MB_LLM_API_KEY"
          autoComplete="off"
          hint={SECRET_REF_HINT}
        />
        <div className="flex items-center gap-2">
          <input
            id="profile-json-schema"
            type="checkbox"
            checked={jsonSchema}
            onChange={(e) => setJsonSchema(e.target.checked)}
          />
          <label htmlFor="profile-json-schema" className="text-sm font-medium">
            JSON schema responses
          </label>
        </div>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
          <Field
            id="profile-temperature"
            label="Temperature"
            type="number"
            min={0}
            max={2}
            step="any"
            value={temperature}
            onChange={(e) => setTemperature(e.target.value)}
            required
          />
          <Field
            id="profile-timeout"
            label="Timeout (ms)"
            type="number"
            min={1}
            step={1}
            value={timeoutMs}
            onChange={(e) => setTimeoutMs(e.target.value)}
            required
          />
          <Field
            id="profile-max-tokens"
            label="Max tokens"
            type="number"
            min={1}
            step={1}
            value={maxTokens}
            onChange={(e) => setMaxTokens(e.target.value)}
            required
          />
        </div>
        <div className="space-y-1">
          <label htmlFor="profile-fallback" className="block text-sm font-medium">
            Fallback profile
          </label>
          <select
            id="profile-fallback"
            className={inputClass}
            value={fallback}
            onChange={(e) => setFallback(e.target.value)}
          >
            <option value="">None</option>
            {fallbacks.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </div>
        <ProblemAlert error={put.error} focus />
        <div className="flex justify-end gap-2">
          <button type="button" className={secondaryButton} onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className={primaryButton} disabled={put.isPending}>
            Save
          </button>
        </div>
      </form>
    </Drawer>
  );
}
