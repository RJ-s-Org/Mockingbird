import { useNavigate } from "@tanstack/react-router";
import { useState, type FormEvent } from "react";
import { rememberWorkspace, useCreateWorkspace, useWorkspaces } from "../api/queries";
import { Dialog } from "./ConfirmDialog";
import { ProblemAlert } from "./ProblemAlert";
import { Field, focusRing, ghostButton, inputClass, primaryButton, secondaryButton } from "./ui";

/** Picks the current workspace; a sysadmin can also create one. */
export function WorkspaceSwitcher({
  current,
  missing = false,
  sysadmin,
}: {
  current?: string;
  /** `current` is not an existing workspace: it is shown as such, not as another one. */
  missing?: boolean;
  sysadmin: boolean;
}) {
  const workspaces = useWorkspaces();
  const navigate = useNavigate();
  const [creating, setCreating] = useState(false);

  const open = (ws: string) => {
    rememberWorkspace(ws);
    void navigate({ to: "/w/$ws/mocks", params: { ws } });
  };

  return (
    <div className="flex min-w-0 flex-wrap items-center gap-2">
      <label htmlFor="workspace-switcher" className="sr-only text-sm text-fg-muted sm:not-sr-only">
        Workspace
      </label>
      <select
        id="workspace-switcher"
        className={`max-w-[11rem] min-w-0 rounded-md border border-line-strong bg-surface px-2 py-1 text-sm font-medium text-fg shadow-xs sm:max-w-xs ${focusRing}`}
        value={current ?? ""}
        onChange={(e) => open(e.target.value)}
        disabled={!workspaces.data}
      >
        {current === undefined ? <option value="">Choose…</option> : null}
        {!workspaces.data && current !== undefined ? <option value={current}>{current}</option> : null}
        {workspaces.data && missing && current !== undefined ? (
          <option value={current} disabled>
            {`${current} (not found)`}
          </option>
        ) : null}
        {(workspaces.data ?? []).map((w) => (
          <option key={w.name} value={w.name}>
            {w.display_name}
          </option>
        ))}
      </select>
      {sysadmin ? (
        <button type="button" className={`${ghostButton} px-2 py-1`} onClick={() => setCreating(true)}>
          <svg aria-hidden="true" viewBox="0 0 16 16" className="size-3.5 fill-current">
            <path d="M7.25 2h1.5v5.25H14v1.5H8.75V14h-1.5V8.75H2v-1.5h5.25z" />
          </svg>
          New workspace
        </button>
      ) : null}
      {creating ? (
        <NewWorkspaceDialog
          onClose={() => setCreating(false)}
          onCreated={(ws) => {
            setCreating(false);
            open(ws);
          }}
        />
      ) : null}
    </div>
  );
}

function NewWorkspaceDialog({ onClose, onCreated }: { onClose: () => void; onCreated: (ws: string) => void }) {
  const create = useCreateWorkspace();
  const [name, setName] = useState("");
  const [displayName, setDisplayName] = useState("");

  const submit = (e: FormEvent) => {
    e.preventDefault();
    create.mutate(
      { name, display_name: displayName.trim() || null },
      { onSuccess: (w) => onCreated(w.name) },
    );
  };

  return (
    <Dialog title="New workspace" onClose={onClose}>
      <form onSubmit={submit} className="space-y-4">
        <Field id="new-workspace-name" label="Name" hint="Lowercase letters, digits and dashes.">
          <input
            id="new-workspace-name"
            className={inputClass}
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
            pattern="[a-z0-9][a-z0-9\-]{0,62}"
            aria-describedby="new-workspace-name-hint"
          />
        </Field>
        <Field id="new-workspace-display" label="Display name">
          <input
            id="new-workspace-display"
            className={inputClass}
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
          />
        </Field>
        <ProblemAlert error={create.error} focus />
        <div className="flex justify-end gap-2 pt-2">
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
