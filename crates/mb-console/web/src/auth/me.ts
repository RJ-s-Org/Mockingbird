import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { api, unwrap } from "../api/client";
import type { components } from "../api/schema";

export type Me = components["schemas"]["MeView"];

export const meQuery = {
  queryKey: ["me"] as const,
  queryFn: async (): Promise<Me> => unwrap(await api.GET("/api/v1/me")),
  staleTime: 60_000,
};

/** The signed-in principal (`GET /api/v1/me`). */
export function useMe(): UseQueryResult<Me> {
  return useQuery(meQuery);
}
