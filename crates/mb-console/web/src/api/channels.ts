import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, unwrap } from "./client";
import type { components } from "./schema";

export type ChannelView = components["schemas"]["ChannelView"];
export type ChannelDetail = components["schemas"]["ChannelDetail"];
export type ChannelActivity = components["schemas"]["ChannelActivityView"];
export type PublishResult = components["schemas"]["PublishResult"];
export type PublishedMessage = components["schemas"]["PublishedMessage"];
export type EventSettings = components["schemas"]["EventSettingsView"];
export type Broker = components["schemas"]["BrokerView"];

/** Which resource changes fire an `on_state_change` trigger. */
export type StateChange = "upserted" | "deleted" | "any";

/** A channel trigger as the editor holds it. */
export type TriggerDraft =
  | { kind: "schedule"; every_ms: number }
  | { kind: "cron"; expr: string }
  | { kind: "on_state_change"; resource_type: string; change: StateChange }
  | { kind: "manual" };

/** The shortest schedule the editor accepts, in milliseconds. */
export const MIN_SCHEDULE_MS = 100;

/** A draft as MCM trigger JSON (the shape `PUT …/channels/{channel}` takes). */
export function toMcmTrigger(t: TriggerDraft): unknown {
  switch (t.kind) {
    case "schedule":
      return { schedule: { every_ms: t.every_ms } };
    case "cron":
      return { cron: { expr: t.expr } };
    case "on_state_change":
      return { on_state_change: { resource_type: t.resource_type, kind: t.change } };
    case "manual":
      return "manual";
  }
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isChange(v: unknown): v is StateChange {
  return v === "upserted" || v === "deleted" || v === "any";
}

/** MCM trigger JSON as a draft; `null` for anything else. */
export function fromMcmTrigger(v: unknown): TriggerDraft | null {
  if (v === "manual") return { kind: "manual" };
  if (!isObject(v)) return null;
  const [key, ...rest] = Object.keys(v);
  if (key === undefined || rest.length > 0) return null;
  const body = v[key];
  if (!isObject(body)) return null;
  switch (key) {
    case "schedule":
      return typeof body.every_ms === "number" ? { kind: "schedule", every_ms: body.every_ms } : null;
    case "cron":
      return typeof body.expr === "string" ? { kind: "cron", expr: body.expr } : null;
    case "on_state_change": {
      if (typeof body.resource_type !== "string") return null;
      const change = body.kind ?? "any";
      return isChange(change) ? { kind: "on_state_change", resource_type: body.resource_type, change } : null;
    }
    default:
      return null;
  }
}

/** A draft's validation error, if any. */
export function triggerError(t: TriggerDraft): string | undefined {
  switch (t.kind) {
    case "schedule":
      if (!Number.isFinite(t.every_ms) || t.every_ms < MIN_SCHEDULE_MS) return `at least ${MIN_SCHEDULE_MS} ms`;
      // The API takes whole milliseconds and refuses a fraction.
      return Number.isInteger(t.every_ms) ? undefined : `a whole number of ms, at least ${MIN_SCHEDULE_MS}`;
    case "cron":
      return t.expr.trim() === "" ? "a cron expression is required" : undefined;
    case "on_state_change":
      return t.resource_type.trim() === "" ? "a resource type is required" : undefined;
    case "manual":
      return undefined;
  }
}

function duration(ms: number): string {
  if (ms >= 3_600_000 && ms % 3_600_000 === 0) return `${ms / 3_600_000}h`;
  if (ms >= 60_000 && ms % 60_000 === 0) return `${ms / 60_000}m`;
  if (ms >= 1000 && ms % 1000 === 0) return `${ms / 1000}s`;
  return `${ms}ms`;
}

/** A trigger in a few words, e.g. `every 5s`, `cron 0 * * * *`, `on order upserted`, `manual`. */
export function summarizeTrigger(v: unknown): string {
  const t = fromMcmTrigger(v);
  if (!t) return JSON.stringify(v);
  switch (t.kind) {
    case "schedule":
      return `every ${duration(t.every_ms)}`;
    case "cron":
      return `cron ${t.expr}`;
    case "on_state_change":
      return `on ${t.resource_type} ${t.change === "any" ? "changed" : t.change}`;
    case "manual":
      return "manual";
  }
}

const channelKeys = {
  channels: (ws: string, mock: string) => ["ws", ws, "mocks", mock, "channels"] as const,
  channel: (ws: string, mock: string, id: string) => ["ws", ws, "mocks", mock, "channels", id] as const,
  eventSettings: (ws: string, mock: string) => ["ws", ws, "mocks", mock, "event-settings"] as const,
  brokers: ["brokers"] as const,
};

/** The channels of a mock's active contract; refreshed every 5 s for the activity counters. */
export function useChannels(ws: string, mock: string) {
  return useQuery({
    queryKey: channelKeys.channels(ws, mock),
    queryFn: async (): Promise<ChannelView[]> =>
      unwrap(await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/channels", { params: { path: { ws, mock } } })),
    refetchInterval: 5000,
  });
}

/** One channel with its schemas; idle while `id` is unknown. */
export function useChannel(ws: string, mock: string, id: string | undefined) {
  return useQuery({
    queryKey: channelKeys.channel(ws, mock, id ?? ""),
    queryFn: async (): Promise<ChannelDetail> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/channels/{channel}", {
          params: { path: { ws, mock, channel: id ?? "" } },
        }),
      ),
    enabled: id !== undefined,
  });
}

/** `PUT …/channels/{channel}`: `triggers: null` uses the contract's; refreshes the channels. */
export function usePutChannel(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({
      id,
      enabled,
      triggers,
    }: {
      id: string;
      enabled: boolean;
      triggers: unknown[] | null;
    }): Promise<ChannelView> =>
      unwrap(
        await api.PUT("/api/v1/workspaces/{ws}/mocks/{mock}/channels/{channel}", {
          params: { path: { ws, mock, channel: id } },
          // MCM triggers are objects or the string "manual"; the schema only knows objects.
          body: { enabled, triggers: triggers as components["schemas"]["PutChannel"]["triggers"] },
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: channelKeys.channels(ws, mock) }),
  });
}

/** `POST …/channels/{channel}/publish`: generates and publishes `count` messages. */
export function usePublish(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({
      id,
      count,
      context,
    }: {
      id: string;
      count: number;
      context?: Record<string, unknown>;
    }): Promise<PublishResult> =>
      unwrap(
        await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/channels/{channel}/publish", {
          params: { path: { ws, mock, channel: id } },
          body: { count, context: context as Record<string, never> | undefined },
        }),
      ),
    onSettled: () => queryClient.invalidateQueries({ queryKey: channelKeys.channels(ws, mock) }),
  });
}

/** A mock's event settings (its broker). */
export function useEventSettings(ws: string, mock: string) {
  return useQuery({
    queryKey: channelKeys.eventSettings(ws, mock),
    queryFn: async (): Promise<EventSettings> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/event-settings", { params: { path: { ws, mock } } }),
      ),
  });
}

/** `PUT …/event-settings`: `broker: null` picks one automatically. */
export function usePutEventSettings(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({ broker }: { broker: string | null }): Promise<EventSettings> =>
      unwrap(
        await api.PUT("/api/v1/workspaces/{ws}/mocks/{mock}/event-settings", {
          params: { path: { ws, mock } },
          body: { broker },
        }),
      ),
    onSuccess: (saved) => queryClient.setQueryData(channelKeys.eventSettings(ws, mock), saved),
  });
}

/** The configured brokers, sorted by name. */
export function useBrokers() {
  return useQuery({
    queryKey: channelKeys.brokers,
    queryFn: async (): Promise<Broker[]> => unwrap(await api.GET("/api/v1/brokers")),
  });
}
