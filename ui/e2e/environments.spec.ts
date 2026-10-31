// U10: explicit binding and shared-action edits stay in the environment's page flow.
import { expect, test } from "@playwright/test";

test("U10 binds a repository environment explicitly and declares one exact shared Flow", async ({ page }) => {
  let environment = {
    name: "production",
    source: "kiln",
    kind: null,
    targets: {},
    protection: { reviewers: ["alice"] } as Record<string, unknown>,
    url: null,
    agents_can_approve: false,
    concurrency: 1,
    latest_deployment: null,
    identity: { scope: { kind: "repository", id: "repo" }, name: "production" },
  };
  const saved: unknown[] = [];
  const flows: unknown[] = [];
  await page.route("**/api/ci/environments**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith("/flows")) {
      flows.push(route.request().postDataJSON());
      return route.fulfill({ json: { id: "flow-123", environment: { scope: { kind: "server" }, name: "release-prod" }, run: "run-1", app: "web", version: "v1" } });
    }
    if (path.endsWith("/deployments")) return route.fulfill({ json: { deployments: [] } });
    if (route.request().method() === "PUT") {
      const body = route.request().postDataJSON();
      saved.push(body);
      environment = { ...environment, ...body, protection: { ...body.protection, approvals: { count: 2, reviewers: ["alice", "bob"] } }, identity: body.protection.policy };
      return route.fulfill({ json: environment });
    }
    return route.fulfill({ json: path.endsWith("/environments") ? { environments: [environment] } : environment });
  });
  await page.route("**/api/deploy/environments", (route) => route.fulfill({ json: { environments: [{ name: "release-prod" }] } }));
  await page.goto("/deployments/environments/production?repo=acme%2Fweb");
  await page.getByLabel("Policy owner", { exact: true }).selectOption("server");
  await page.getByLabel("Server environment", { exact: true }).selectOption("release-prod");
  await expect(page.getByLabel("Required reviewers", { exact: true })).toBeDisabled();
  await page.getByRole("button", { name: "Save production", exact: true }).click();
  await expect.poll(() => saved.length).toBe(1);
  expect((saved[0] as { protection: unknown }).protection).toEqual({ policy: { scope: { kind: "server" }, name: "release-prod" } });
  await page.getByLabel("CI run", { exact: true }).fill("run-1");
  await page.getByLabel("Release app", { exact: true }).fill("web");
  await page.getByLabel("Exact release version", { exact: true }).fill("v1");
  await page.getByRole("button", { name: "Declare shared action", exact: true }).click();
  await expect(page.getByLabel("Release protected_action", { exact: true })).toHaveValue("flow-123");
  expect(flows).toEqual([{ run: "run-1", app: "web", version: "v1" }]);
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth)).toBe(false);
});

test("U10 edits the one shared CI/release capacity on the server policy", async ({ page }) => {
  let environment = {
    name: "production",
    provider: "kilncd",
    release_repo: { host: "kiln", repo: "kiln/releases", branch: "main", root: "" },
    targets: { kubernetes: [] },
    gates: { concurrency_limit: 1 },
    concurrency: "queue",
    agents: { deploy: false, approve: false },
    owners: { release: false },
    argocd: { applications: false },
  };
  const saved: unknown[] = [];
  await page.route("**/api/deploy/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/deploy/environments/production") {
      if (route.request().method() === "PUT") {
        const body = route.request().postDataJSON();
        saved.push(body);
        environment = body;
      }
      return route.fulfill({ json: environment });
    }
    if (path.endsWith("/board")) return route.fulfill({ json: { environments: [environment], apps: [], cells: [], waiting_approval: [] } });
    if (path.endsWith("/git-hosts")) return route.fulfill({ json: { hosts: [] } });
    if (path.includes("/move")) return route.fulfill({ json: { move: null } });
    return route.fulfill({ json: { environments: [environment], releases: [] } });
  });
  await page.goto("/deployments/releases/environments/production");
  await page.getByLabel("Shared capacity", { exact: true }).fill("2");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect.poll(() => saved.length).toBe(1);
  expect((saved[0] as { gates: { concurrency_limit: number } }).gates.concurrency_limit).toBe(2);
  expect(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth)).toBe(false);
});

// W23-R05/R07/W15-R09: provider ownership and provider errors stay explicit on the board.
test("provider environments and read errors render without local policy controls", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/repos", (route) => route.fulfill({ json: [{ id: "repo", name: "acme/provider" }] }));
  await page.route("**/api/ci/environments/board", (route) =>
    route.fulfill({
      json: {
        providers: { "acme/provider": ["bitbucket-pipelines", "github-actions"] },
        environments: [
          {
            repo: "acme/provider",
            provider: "bitbucket-pipelines",
            name: "Production",
            url: "https://bitbucket.example/environments/production",
            waiting: [],
            deployment: { state: "COMPLETED", result: "SUCCESSFUL", commit: "deadbeef01234567", url: "https://bitbucket.example/deployments/7" },
          },
          { repo: "acme/provider", provider: "github-actions", error: "GitHub temporarily unavailable", waiting: [] },
        ],
      },
    }),
  );
  await page.goto("/deployments/environments");
  const rows = page.locator(".env-rows");
  const provider = rows.locator(".env-row").filter({ hasText: "Bitbucket Pipelines" });
  await expect(provider).toContainText("SUCCESSFUL");
  await expect(provider).toContainText("deadbee");
  await expect(provider.getByRole("link", { name: "Production", exact: true })).toHaveAttribute("href", "https://bitbucket.example/environments/production");
  await expect(provider.getByRole("link", { name: "its deployment", exact: true })).toHaveAttribute("href", "https://bitbucket.example/deployments/7");
  await expect(provider).toContainText("Deployment and protection are managed by Bitbucket Pipelines.");
  await expect(rows.locator(".env-row").filter({ hasText: "GitHub Actions" })).toContainText("GitHub temporarily unavailable");
  await expect(rows).not.toContainText("anyone may deploy");
  await expect(rows.locator('a[href^="/deployments/environments/"]')).toHaveCount(0);
  await expect(rows.getByRole("button")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Make dev, staging and production", exact: true })).toHaveCount(0);
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(errors).toEqual([]);
  expect(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth)).toBe(false);
});
