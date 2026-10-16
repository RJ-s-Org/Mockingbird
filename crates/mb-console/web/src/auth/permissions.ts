import type { Me } from "./me";

export type Role = "viewer" | "editor" | "admin" | "owner";
export type Permission = "read" | "edit" | "manageProfiles" | "manageApiKeys" | "manageWorkspace";

const RANK: Record<Role, number> = { viewer: 0, editor: 1, admin: 2, owner: 3 };

/** The minimum role for each permission, as the server enforces it. */
const MINIMUM: Record<Permission, Role> = {
  read: "viewer",
  edit: "editor",
  manageProfiles: "admin",
  manageApiKeys: "admin",
  manageWorkspace: "owner",
};

/** The caller's role in `ws`; a sysadmin is `owner` everywhere. */
export function roleIn(me: Me, ws: string): Role | undefined {
  if (me.sysadmin) return "owner";
  return me.workspaces.find((w) => w.name === ws)?.role;
}

/** Whether `me` may do `p` in workspace `ws` (only a UI hint: the server decides). */
export function can(me: Me, ws: string, p: Permission): boolean {
  const role = roleIn(me, ws);
  return role !== undefined && RANK[role] >= RANK[MINIMUM[p]];
}
