import { useEffect, useState, type FormEvent } from "react";
import { useCreateApiKey, type CreateApiKey, type CreatedApiKey, type KeyScope, type Role } from "../api/queries";
import { Dialog } from "../components/ConfirmDialog";
import { ProblemAlert } from "../components/ProblemAlert";
import { Checkbox, Field, Notice, hintClass, inputClass, primaryButton, secondaryButton } from "../components/ui";

const ROLES: Role[] = ["viewer", "editor", "admin", "owner"];
const SCOPES: KeyScope[] = ["admin", "data-plane"];

/** The roles a caller with `role` may give a key: at most their own. */
export function grantableRoles(role: Role): Role[] {
  return ROLES.slice(0, ROLES.indexOf(role) + 1);
}

/**
 * Creates an API key in `ws`, then shows its token once. Closing forgets
 * the token: it is never shown again.
 */
export function CreateKeyDialog({
  ws,
  myRole,
  sysadmin,
  onClose,
}: {
  ws: string;
  /** The caller's role in `ws`; the key's role is capped at it. */
  myRole: Role;
  /** Whether the caller may create a sysadmin key. */
  sysadmin: boolean;
  onClose: () => void;
}) {
  const create = useCreateApiKey(ws);
  const [created, setCreated] = useState<CreatedApiKey | null>(null);
  const [name, setName] = useState("");
  const [role, setRole] = useState<Role>("viewer");
  const [scopes, setScopes] = useState<KeyScope[]>(["admin"]);
  const [expires, setExpires] = useState("");
  const [asSysadmin, setAsSysadmin] = useState(false);
  const [copied, setCopied] = useState(false);

  // However the dialog goes away, the mutation (and its copy of the token)
  // is released so the cache can drop it.
  const { reset } = create;
  useEffect(() => reset, [reset]);

  const close = () => {
    // Drop every copy of the token: ours and the mutation's.
    setCreated(null);
    create.reset();
    onClose();
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const body: CreateApiKey = { name: name.trim(), role, scopes };
    // The key works until the end of the chosen day (UTC).
    if (expires) body.expires_at = `${expires}T23:59:59Z`;
    if (asSysadmin) body.sysadmin = true;
    create.mutate(body, {
      onSuccess: (r) => {
        setCreated(r);
        create.reset();
      },
    });
  };

  const copy = async () => {
    if (!created) return;
    await navigator.clipboard.writeText(created.token);
    setCopied(true);
  };

  if (created) {
    return (
      <Dialog title="API key created" onClose={close}>
        <div className="space-y-4">
          <Notice tone="warn" role="alert" className="font-medium">
            This token will not be shown again
          </Notice>
          <p className="text-sm text-fg-muted">
            Key <code>{created.key.key_id}</code> ({created.key.name}). Copy the token now and store it safely.
          </p>
          <code
            data-testid="api-key-token"
            className="block rounded-md border border-line bg-surface-2 p-3 font-mono text-sm break-all text-fg"
          >
            {created.token}
          </code>
          <div className="flex items-center justify-end gap-2 pt-2">
            {copied ? (
              <span role="status" className="text-sm font-medium text-ok-fg">
                Copied.
              </span>
            ) : null}
            <button type="button" className={secondaryButton} onClick={() => void copy()}>
              Copy
            </button>
            <button type="button" className={primaryButton} onClick={close}>
              Close
            </button>
          </div>
        </div>
      </Dialog>
    );
  }

  const toggleScope = (scope: KeyScope, on: boolean) =>
    setScopes((s) => (on ? SCOPES.filter((x) => x === scope || s.includes(x)) : s.filter((x) => x !== scope)));

  return (
    <Dialog title="New API key" onClose={close}>
      <form onSubmit={submit} className="space-y-5">
        <Field id="key-name" label="Name">
          <input
            id="key-name"
            className={inputClass}
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
            maxLength={128}
          />
        </Field>
        <Field id="key-role" label="Role">
          <select
            id="key-role"
            className={inputClass}
            value={role}
            onChange={(e) => setRole(e.target.value as Role)}
          >
            {grantableRoles(myRole).map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
        </Field>
        <fieldset className="space-y-2">
          <legend className="mb-1.5 text-sm font-medium">Scopes</legend>
          {SCOPES.map((scope) => (
            <Checkbox
              key={scope}
              id={`key-scope-${scope}`}
              label={scope}
              checked={scopes.includes(scope)}
              onChange={(on) => toggleScope(scope, on)}
            />
          ))}
          {scopes.length === 0 ? (
            <p className="text-xs text-bad-fg">Choose at least one scope.</p>
          ) : (
            <p className={hintClass}>admin: the Admin API. data-plane: mocks with api_key auth.</p>
          )}
        </fieldset>
        <Field id="key-expires" label="Expires on (optional)">
          <input
            id="key-expires"
            type="date"
            className={inputClass}
            value={expires}
            onChange={(e) => setExpires(e.target.value)}
          />
        </Field>
        {sysadmin ? (
          <Checkbox
            id="key-sysadmin"
            label="Sysadmin key (acts as a sysadmin in every workspace)"
            checked={asSysadmin}
            onChange={setAsSysadmin}
          />
        ) : null}
        <ProblemAlert error={create.error} focus />
        <div className="flex justify-end gap-2 pt-2">
          <button type="button" className={secondaryButton} onClick={close}>
            Cancel
          </button>
          <button type="submit" className={primaryButton} disabled={create.isPending || scopes.length === 0}>
            Create
          </button>
        </div>
      </form>
    </Dialog>
  );
}
