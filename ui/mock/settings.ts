/**
 * Settings → Deployments in the mock: where images live, the registries and the accounts kiln
 * deploys through (GET/PUT /api/cd/images, /api/ci/integrations, /api/ci/oidc), kept in memory.
 */
import type http from "node:http";
import type { CdImages, ImageHome, Integration, IntegrationKind, Integrations, OidcIssuer } from "../src/api/client.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, msg: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

const T0 = Math.floor(Date.now() / 1000);

const images: CdImages = {
  images: {
    kiln: {
      kind: "registry",
      repository: "ghcr.io/hyper-light/kiln",
      integration: "ghcr",
    },
  },
  used: { kiln: ["kiln"], "docs-site": ["docs"] },
};

const kinds: IntegrationKind[] = [
  {
    kind: "oci",
    title: "Container registry",
    category: "registry",
    mode: "optional",
    fields: [{ name: "registry", required: true, help: "docker.io" }],
    secret: "Token",
    built: true,
  },
  {
    kind: "aws",
    title: "AWS",
    category: "cloud",
    mode: "oidc",
    fields: [
      {
        name: "role_arn",
        required: true,
        help: "arn:aws:iam::123456789012:role/kiln-deploy",
      },
    ],
    secret: null,
    built: true,
  },
  {
    kind: "gcp",
    title: "Google Cloud",
    category: "cloud",
    mode: "oidc",
    fields: [
      { name: "provider", required: true, help: "projects/…/providers/kiln" },
      {
        name: "service_account",
        required: true,
        help: "kiln@project.iam.gserviceaccount.com",
      },
    ],
    secret: null,
    built: true,
  },
  {
    kind: "cloudflare",
    title: "Cloudflare",
    category: "platform",
    mode: "token",
    fields: [{ name: "account_id", required: true, help: "the account's id" }],
    secret: "API token",
    built: true,
  },
  {
    kind: "fly",
    title: "Fly.io",
    category: "platform",
    mode: "token",
    fields: [{ name: "org", required: false, help: "personal" }],
    secret: "Access token",
    built: false,
  },
];

let integrations: Integration[] = [
  {
    id: "i1",
    kind: "oci",
    name: "ghcr",
    config: { registry: "ghcr.io", username: "ada" },
    has_secret: true,
    scope: "server",
    created_at: T0 - 86_400 * 9,
    verified_at: T0 - 3_600,
    last_error: null,
    last_used_at: T0 - 600,
  },
  {
    id: "i2",
    kind: "oci",
    name: "dockerhub",
    config: { registry: "docker.io" },
    has_secret: false,
    scope: "server",
    created_at: T0 - 86_400 * 30,
    verified_at: null,
    last_error: null,
    last_used_at: null,
  },
  {
    id: "i3",
    kind: "aws",
    name: "aws-production",
    config: { role_arn: "arn:aws:iam::123456789012:role/kiln-deploy" },
    has_secret: false,
    scope: "server",
    created_at: T0 - 86_400 * 4,
    verified_at: T0 - 86_400,
    last_error: null,
    last_used_at: T0 - 7_200,
  },
  {
    id: "i4",
    kind: "cloudflare",
    name: "cf-sites",
    config: { account_id: "9a7b…" },
    has_secret: true,
    scope: "server",
    created_at: T0 - 86_400 * 2,
    verified_at: null,
    last_error: "the token can't list Pages projects (403)",
    last_used_at: null,
  },
];

const oidc: OidcIssuer = {
  issuer: "https://kiln.example.com/oidc",
  discovery: "https://kiln.example.com/oidc/.well-known/openid-configuration",
  jwks_uri: "https://kiln.example.com/oidc/jwks",
  serving: true,
};

/** Handles the routes above; false when it doesn't know the route. */
export async function handleSettings(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  let mm: RegExpMatchArray | null;
  if (p === "/api/cd/images" && m === "GET") return (ctx.json(res, 200, images), true);
  if ((mm = p.match(/^\/api\/cd\/images\/([^/]+)$/)) && m === "PUT") {
    images.images[decodeURIComponent(mm[1]!)] = await ctx.body<ImageHome>(req);
    return (ctx.json(res, 200, images), true);
  }
  // kiln's GitHub App (docs/ci.md W14-R10): none until GitHub's manifest flow makes one.
  if (p === "/api/ci/github/app" && m === "GET") return (ctx.json(res, 200, { configured: false, web_url: "https://github.com" }), true);
  if (p === "/api/ci/github/app/manifest" && m === "POST") {
    const b = await ctx.body<{ org?: string }>(req);
    const action = b.org ? `https://github.com/organizations/${encodeURIComponent(b.org)}/settings/apps/new?state=mock` : "https://github.com/settings/apps/new?state=mock";
    const base = `http://${req.headers.host ?? "127.0.0.1"}`;
    return (ctx.json(res, 200, { action, manifest: JSON.stringify({ name: `kiln (${req.headers.host ?? ""})`, url: base, redirect_url: `${base}/api/ci/github/app/callback`, public: false }) }), true);
  }
  if (p === "/api/ci/integrations" && m === "GET") return (ctx.json(res, 200, { kinds, integrations } satisfies Integrations), true);
  if (p === "/api/ci/integrations" && m === "POST") {
    const b = await ctx.body<{
      kind: string;
      name: string;
      config: Record<string, string>;
      secret?: string;
    }>(req);
    if (integrations.some((i) => i.name === b.name)) return (ctx.err(res, 409, `${b.name} already exists`), true);
    const made: Integration = {
      id: `i${integrations.length + 1}`,
      kind: b.kind,
      name: b.name,
      config: b.config,
      has_secret: Boolean(b.secret),
      scope: "server",
      created_at: Math.floor(Date.now() / 1000),
      verified_at: null,
      last_error: null,
      last_used_at: null,
    };
    integrations = [...integrations, made];
    return (ctx.json(res, 201, made), true);
  }
  if ((mm = p.match(/^\/api\/ci\/integrations\/([^/]+)\/verify$/)) && m === "POST") {
    const name = decodeURIComponent(mm[1]!);
    integrations = integrations.map((i) => (i.name === name ? { ...i, verified_at: Math.floor(Date.now() / 1000), last_error: null } : i));
    return (ctx.json(res, 200, { ok: true, message: "it answered" }), true);
  }
  if ((mm = p.match(/^\/api\/ci\/integrations\/([^/]+)$/)) && m === "DELETE") {
    const name = decodeURIComponent(mm[1]!);
    integrations = integrations.filter((i) => i.name !== name);
    res.statusCode = 204;
    res.end();
    return true;
  }
  if (p === "/api/ci/oidc" && m === "GET") return (ctx.json(res, 200, oidc), true);
  return false;
}
