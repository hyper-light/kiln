import { createServerFn } from "@tanstack/react-start";
import { getRequest, setResponseHeader } from "@tanstack/react-start/server";
import { readPrivateBootstrap } from "./private-bootstrap-data.server";

/** Start owns serialization/escaping. No private data is written into the shared static shell. */
export const getPrivateBootstrap = createServerFn({ method: "GET" })
  .validator((pathname: unknown) => (typeof pathname === "string" && pathname.length <= 2048 ? pathname : ""))
  .handler(async ({ data: pathname }) => {
    setResponseHeader("cache-control", "private, no-store");
    setResponseHeader("vary", "Cookie, Authorization");
    // This browser-typed route tree also compiles server functions. Keep the server runtime's
    // environment shape local rather than adding Node globals to every browser module.
    const runtime = globalThis as typeof globalThis & { process?: { env?: Record<string, string | undefined> } };
    const apiOrigin = runtime.process?.env?.KILN_API_URL ?? "http://127.0.0.1:7070";
    return readPrivateBootstrap(getRequest(), pathname, { apiOrigin });
  });
