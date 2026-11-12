/** Private first-page data, carried by Start's request-scoped loader serialization. */
import type { AgentInfo } from "@/api/harness";
import type { Me } from "@/api/types";

export interface PrivateBootstrap {
  version: 1;
  pathname: string;
  me: Me;
  sessions?: AgentInfo[];
}

/** Public pages and peered clusters keep their existing independent authentication path. */
export function privatePage(pathname: string): boolean {
  return pathname.startsWith("/") && !pathname.startsWith("//") && !/^\/(?:shared|\.kiln-peer|_serverFn)(?:\/|$)/.test(pathname);
}

export function needsSessions(pathname: string): boolean {
  return /^\/agents(?:\/?$|\/(?!(?:plans|research|sessions|agentfiles)(?:\/|$))[^/]+\/?$)/.test(pathname);
}

/** A bootstrap belongs only to this initial page, never another route or a later login. */
export function bootstrapForPage(value: PrivateBootstrap | null, pathname: string): value is PrivateBootstrap {
  return !!value && value.version === 1 && privatePage(pathname) && value.pathname === pathname && typeof value.me?.authenticated === "boolean";
}
