import { useState, type FormEvent } from "react";
import { useCreateMock, useProfiles, type Mock } from "../../api/queries";
import { Dialog, inputClass, primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";

/** Creates a mock in `ws`; a 409 or 422 problem stays inline. */
export function CreateMockDialog({
  ws,
  onClose,
  onCreated,
}: {
  ws: string;
  onClose: () => void;
  onCreated: (mock: Mock) => void;
}) {
  const create = useCreateMock(ws);
  const profiles = useProfiles(ws);
  const [name, setName] = useState("");
  const [prefix, setPrefix] = useState("");
  const [persona, setPersona] = useState("");
  const [profile, setProfile] = useState("");
  const [headerOverrides, setHeaderOverrides] = useState(false);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    create.mutate(
      {
        name,
        route_prefix: prefix.trim() || null,
        persona: persona.trim() || null,
        llm_profile: profile || null,
        header_overrides: headerOverrides,
      },
      { onSuccess: onCreated },
    );
  };

  return (
    <Dialog title="New mock" onClose={onClose}>
      <form onSubmit={submit} className="space-y-4">
        <div className="space-y-1">
          <label htmlFor="new-mock-name" className="block text-sm font-medium">
            Name
          </label>
          <input
            id="new-mock-name"
            className={inputClass}
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
            pattern="[a-z0-9][a-z0-9\-]{0,62}"
            aria-describedby="new-mock-name-hint"
          />
          <p id="new-mock-name-hint" className="text-xs text-slate-600">
            Lowercase letters, digits and dashes.
          </p>
        </div>
        <div className="space-y-1">
          <label htmlFor="new-mock-prefix" className="block text-sm font-medium">
            Route prefix
          </label>
          <input
            id="new-mock-prefix"
            className={inputClass}
            value={prefix}
            placeholder={`/${name || "<name>"}`}
            onChange={(e) => setPrefix(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <label htmlFor="new-mock-persona" className="block text-sm font-medium">
            Persona
          </label>
          <textarea
            id="new-mock-persona"
            className={inputClass}
            rows={3}
            value={persona}
            onChange={(e) => setPersona(e.target.value)}
          />
        </div>
        <ProfileSelect id="new-mock-profile" value={profile} onChange={setProfile} profiles={profiles.data} />
        <div className="flex items-center gap-2">
          <input
            id="new-mock-overrides"
            type="checkbox"
            checked={headerOverrides}
            onChange={(e) => setHeaderOverrides(e.target.checked)}
          />
          <label htmlFor="new-mock-overrides" className="text-sm font-medium">
            Header overrides
          </label>
        </div>
        <ProblemAlert error={create.error} />
        <div className="flex justify-end gap-2">
          <button type="button" className={secondaryButton} onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className={primaryButton} disabled={create.isPending}>
            Create
          </button>
        </div>
      </form>
    </Dialog>
  );
}

/** "LLM profile": none, or one of the workspace's profiles. */
export function ProfileSelect({
  id,
  value,
  onChange,
  profiles,
}: {
  id: string;
  value: string;
  onChange: (profile: string) => void;
  profiles?: { name: string }[];
}) {
  // Keep a profile that is not (or not yet) listed selectable.
  const names = (profiles ?? []).map((p) => p.name);
  if (value && !names.includes(value)) names.unshift(value);
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-sm font-medium">
        LLM profile
      </label>
      <select id={id} className={inputClass} value={value} onChange={(e) => onChange(e.target.value)}>
        <option value="">None</option>
        {names.map((n) => (
          <option key={n} value={n}>
            {n}
          </option>
        ))}
      </select>
    </div>
  );
}
