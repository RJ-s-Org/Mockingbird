import { useState, type FormEvent } from "react";
import { useCreateMock, useProfiles, type Mock } from "../../api/queries";
import { Dialog, inputClass, primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";
import { NAME_PATTERN, NAME_RULE, isValidName } from "../../lib/names";

/**
 * Creates a mock in `ws`. The name is checked against the server's rule as it
 * is typed; a 409 or 422 problem from the server stays inline (its title and
 * detail, whatever they are) until a field changes.
 */
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
  // On by default, like the API's `header_overrides` and the config file.
  const [headerOverrides, setHeaderOverrides] = useState(true);

  const nameValid = isValidName(name);
  const nameInvalid = name !== "" && !nameValid;
  /** Wraps a field's setter: editing clears a stale server error. */
  const edit =
    <V,>(set: (v: V) => void) =>
    (v: V) => {
      if (create.error) create.reset();
      set(v);
    };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!nameValid) return;
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
            onChange={(e) => edit(setName)(e.target.value)}
            required
            pattern={NAME_PATTERN}
            aria-invalid={nameInvalid}
            aria-describedby="new-mock-name-hint"
          />
          <p id="new-mock-name-hint" className={`text-xs ${nameInvalid ? "text-red-800 dark:text-red-300" : "text-slate-600 dark:text-slate-400"}`}>
            {nameInvalid ? `Invalid name. ${NAME_RULE}` : NAME_RULE}
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
            placeholder={nameValid ? `/${name}` : "/<name>"}
            aria-describedby="new-mock-prefix-hint"
            onChange={(e) => edit(setPrefix)(e.target.value)}
          />
          <p id="new-mock-prefix-hint" className="text-xs text-slate-600 dark:text-slate-400">
            Defaults to a slash followed by the name.
          </p>
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
            onChange={(e) => edit(setPersona)(e.target.value)}
          />
        </div>
        <ProfileSelect id="new-mock-profile" value={profile} onChange={edit(setProfile)} profiles={profiles.data} />
        <div className="flex items-center gap-2">
          <input
            id="new-mock-overrides"
            type="checkbox"
            checked={headerOverrides}
            onChange={(e) => edit(setHeaderOverrides)(e.target.checked)}
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
          <button type="submit" className={primaryButton} disabled={create.isPending || nameInvalid}>
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
