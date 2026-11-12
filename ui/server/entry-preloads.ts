/** W21-R02: request-local hints for public modules; never prefetch data or run route loaders. */
export function agentEntry(pathname: string): boolean {
  return /^\/agents\/(?!(?:plans|research|sessions)(?:\/|$))[^/]+\/?$/.test(pathname);
}

export function preloadLinks(manifest: unknown): string {
  if (!manifest || typeof manifest !== "object") return "";
  const value = manifest as { version?: unknown; agent?: unknown };
  if (value.version !== 1 || !Array.isArray(value.agent) || value.agent.length > 128) return "";
  const paths = value.agent;
  if (!paths.every((p): p is string => typeof p === "string" && p.length <= 256 && /^\/assets\/[A-Za-z0-9_./~-]+\.js$/.test(p) && !p.includes(".."))) return "";
  return [...new Set(paths)].map((path) => '<link rel="modulepreload" crossorigin="anonymous" href="' + path + '">').join("");
}

/** Insert after the shell's hints so speculative imports cannot take their place in the queue. */
export function withEntryPreloads(response: Response, links: string): Response {
  if (!links || !response.body || !(response.headers.get("content-type") ?? "").includes("text/html") || response.headers.has("content-encoding")) return response;
  const encode = new TextEncoder();
  const decode = new TextDecoder();
  let head = "";
  let passed = false;
  let seen = 0;
  const body = response.body.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, output) {
        if (passed) return output.enqueue(encode.encode(decode.decode(chunk, { stream: true })));
        const text = decode.decode(chunk, { stream: true });
        seen += text.length;
        head += text;
        const match = /<\/head\s*>/i.exec(head);
        const at = match && seen - head.length + match.index < 65536 ? match.index : -1;
        if (at >= 0 || seen >= 65536) {
          passed = true;
          output.enqueue(encode.encode(at >= 0 ? head.slice(0, at) + links + head.slice(at) : head));
          head = "";
          return;
        }
        // Forward existing head hints immediately, retaining only enough text for a split closing
        // tag. Keep a surrogate pair intact when re-encoding a UTF-8 character at the cut.
        let cut = Math.max(0, head.length - 64);
        const previous = head.charCodeAt(cut - 1);
        if (previous >= 0xd800 && previous <= 0xdbff) cut--;
        if (cut > 0) {
          output.enqueue(encode.encode(head.slice(0, cut)));
          head = head.slice(cut);
        }
      },
      flush(output) {
        output.enqueue(encode.encode((passed ? "" : head) + decode.decode()));
      },
    }),
  );
  const headers = new Headers(response.headers);
  headers.delete("content-length");
  headers.delete("etag");
  return new Response(body, { status: response.status, statusText: response.statusText, headers });
}
