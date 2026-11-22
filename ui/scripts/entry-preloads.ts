/** W21-R02: discover the requested agent's public code with its document, without executing it. */
import type { Plugin } from "vite";

interface Chunk {
  fileName: string;
  facadeModuleId: string | null;
  moduleIds: readonly string[];
  imports: readonly string[];
  isEntry: boolean;
}

function routeModules(chunks: readonly Chunk[], routeRe: RegExp, viewRe?: RegExp): string[] {
  const byName = new Map(chunks.map((c) => [c.fileName, c]));
  const isRoute = (id: string) => routeRe.test(id);
  const isShell = (id: string) => /(?:^|[/\\])src[/\\]routes[/\\](?:__root|_app)\.tsx(?:\?|$)/.test(id);
  // The shell contains route registration stubs too. Prefer the split component facade, never
  // mistake the shell's reference to this route for the component that needs discovering.
  const target =
    chunks.find((c) => c.facadeModuleId && isRoute(c.facadeModuleId) && /[?&]tsr-split=component(?:&|$)/.test(c.facadeModuleId)) ??
    (viewRe ? chunks.find((c) => c.moduleIds.some((id) => viewRe.test(id))) : undefined) ??
    chunks.find((c) => !c.isEntry && c.facadeModuleId && isRoute(c.facadeModuleId));
  if (!target) return [];
  const visit = (name: string, seen: Set<string>) => {
    if (seen.has(name)) return;
    const chunk = byName.get(name);
    if (!chunk) return;
    seen.add(name);
    for (const imported of chunk.imports) visit(imported, seen);
  };
  const shell = new Set<string>();
  // Start also hints the shared app/root route components, even when the router splits them
  // away from the client entry. Their static imports are already discovered with every page.
  for (const entry of chunks) if (entry !== target && (entry.isEntry || (entry.facadeModuleId && isShell(entry.facadeModuleId)) || entry.moduleIds.some(isShell))) visit(entry.fileName, shell);
  const needed = new Set<string>();
  visit(target.fileName, needed);
  return [...needed].filter((name) => !shell.has(name)).map((name) => "/" + name);
}

export function agentModules(chunks: readonly Chunk[]): string[] {
  return routeModules(chunks, /(?:^|[/\\])src[/\\]routes[/\\]_app\.agents\.\$id\.tsx(?:\?|$)/, /[/\\]src[/\\]views[/\\]harness[/\\]agent\.tsx(?:\?|$)/);
}

export function entryPreloads(): Plugin {
  return {
    name: "kiln:entry-preloads",
    apply: "build",
    applyToEnvironment: (env) => env.config.consumer === "client",
    generateBundle: {
      order: "post",
      handler(_options, bundle) {
        const chunks = Object.values(bundle).filter((item) => item.type === "chunk");
        // W21-R02: exercise the native retained-asset filename contract against actual emitted
        // chunks, so a new splitting scheme cannot silently break clients across deploys.
        for (const chunk of chunks) if (!/^assets\/[A-Za-z0-9_-][A-Za-z0-9_.-]*\.js$/.test(chunk.fileName)) this.error("Client chunk is incompatible with retained UI assets: " + chunk.fileName);
        this.emitFile({
          type: "asset",
          fileName: "_entry-preloads.json",
          source: JSON.stringify({
            version: 1,
            agent: agentModules(chunks),
            agents: routeModules(chunks, /(?:^|[/\\])src[/\\]routes[/\\]_app\.agents\.index\.tsx(?:\?|$)/, /[/\\]src[/\\]views[/\\]harness[/\\]mission\.tsx(?:\?|$)/),
            ship: routeModules(chunks, /(?:^|[/\\])src[/\\]routes[/\\]_app\.ship(?:\.index)?\.tsx(?:\?|$)/, /[/\\]src[/\\]views[/\\]ship[/\\]index\.tsx(?:\?|$)/),
            runners: routeModules(chunks, /(?:^|[/\\])src[/\\]routes[/\\]_app\.runners(?:\.index)?\.tsx(?:\?|$)/),
          }),
        });
      },
    },
  };
}
