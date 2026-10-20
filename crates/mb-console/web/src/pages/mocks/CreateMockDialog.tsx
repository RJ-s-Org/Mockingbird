import { useState, type FormEvent } from "react";
import { useCreateMock, useProfiles, type Mock } from "../../api/queries";
import { Dialog } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";
import { NAME_PATTERN, NAME_RULE, isValidName } from "../../lib/names";
import { Checkbox, Field, errorTextClass, hintClass, inputClass, labelClass, primaryButton, secondaryButton } from "../../components/ui";

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
      <form onSubmit={submit} className="space-y-5">
        <div className="space-y-1.5">
          <label htmlFor="new-mock-name" className={labelClass}>
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
          <p id="new-mock-name-hint" className={nameInvalid ? errorTextClass : hintClass}>
            {nameInvalid ? `Invalid name. ${NAME_RULE}` : NAME_RULE}
          </p>
        </div>
        <Field id="new-mock-prefix" label="Route prefix" hint="Defaults to a slash followed by the name.">
          <input
            id="new-mock-prefix"
            className={inputClass}
            value={prefix}
            placeholder={nameValid ? `/${name}` : "/<name>"}
            aria-describedby="new-mock-prefix-hint"
            onChange={(e) => edit(setPrefix)(e.target.value)}
          />
        </Field>
        <Field id="new-mock-persona" label="Persona">
          <textarea
            id="new-mock-persona"
            className={inputClass}
            rows={3}
            value={persona}
            onChange={(e) => edit(setPersona)(e.target.value)}
          />
        </Field>
        <ProfileSelect id="new-mock-profile" value={profile} onChange={edit(setProfile)} profiles={profiles.data} />
        <Checkbox
          id="new-mock-overrides"
          label="Header overrides"
          checked={headerOverrides}
          onChange={edit(setHeaderOverrides)}
        />
        <ProblemAlert error={create.error} focus />
        <div className="flex justify-end gap-2 pt-2">
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
    <Field id={id} label="LLM profile">
      <select id={id} className={inputClass} value={value} onChange={(e) => onChange(e.target.value)}>
        <option value="">None</option>
        {names.map((n) => (
          <option key={n} value={n}>
            {n}
          </option>
        ))}
      </select>
    </Field>
  );
}
