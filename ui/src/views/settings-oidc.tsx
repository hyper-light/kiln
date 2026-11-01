/**
 * Settings → CI → OIDC (docs/ci.md W21-R11, W10-R06, R12): kiln's issuer (where it is, whether
 * this server serves its documents, and how providers on the internet reach it), its signing keys
 * (which signs, which are published), rotating and publishing them, the key set to publish
 * elsewhere, and each federating integration's needs and readiness (its last Verify).
 */
import * as React from "react";
import { api, type Integrations } from "@/api/client";
import { ci, type CiOidc } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Confirm, Facts } from "@/components/ui/misc";
import { SettingsGroup } from "@/components/ui/settings";
import { useAction } from "@/lib/action";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";

/** Kinds that trust kiln's tokens instead of a stored secret. */
const FEDERATING = new Set(["aws", "gcp", "azure"]);

/** What each kind needs of the issuer (W10-R12): its keys fetched over the internet, or given to it. */
const FETCHES = "fetches kiln's keys over the internet";
const NEEDS: Record<string, string> = { aws: FETCHES, azure: FETCHES, oci: FETCHES, gcp: "takes kiln's keys pasted into its pool, or fetches them" };

export function OidcHold() {
  return (
    <SettingsGroup id="ci.oidc" title="OIDC" words="oidc issuer keys jwks federation">
      <Hold height={140} />
    </SettingsGroup>
  );
}

export function OidcGroups() {
  const res = useSuspenseResource("ci/oidc/keys", (s) => ci.oidc(s));
  const d: CiOidc = res.data;
  const reach = d.reach;
  const integrations: Integrations = useSuspenseResource("ci/integrations", (s) => api.integrations(s)).data;
  const federated = integrations.integrations.filter((i) => FEDERATING.has(i.kind) || (i.kind === "oci" && typeof i.config.federation === "string"));
  const [said, setSaid] = React.useState<{ tone?: "error"; text: string } | null>(null);
  // The rotate confirm, by its opening: a rotation answered after it was closed and opened again
  // leaves the one open now as it is (W21-R15).
  const [now, setNow] = React.useState<number | null>(null);
  const openings = React.useRef(0);
  const act = async (f: () => Promise<string>) => {
    setSaid(null);
    try {
      setSaid({ text: await f() });
      res.reload();
    } catch (e) {
      setSaid({ tone: "error", text: errorMessage(e) });
    }
  };
  // Publishing and rotating both change the keys kiln's issuer publishes: one at a time (W21-R15),
  // the one pressed saying so and the others waiting, so one message line says how each went.
  const [pressed, setPressed] = React.useState<"publish" | "later" | "now">("publish");
  const [keys, changing] = useAction((what: "publish" | "later" | "now") => {
    setPressed(what);
    if (what === "publish") return act(async () => `Published to ${(await ci.oidcPublish()).published}.`);
    const at = now;
    return act(async () => {
      if (what === "now") {
        await ci.oidcRotate(true);
        return "A new key signs now.";
      }
      return `A new key signs from ${new Date((await ci.oidcRotate(false)).sign_from * 1000).toLocaleString()}.`;
    }).then(() => setNow((n) => (n === at ? null : n)));
  });
  const on = (what: "publish" | "later" | "now") => changing && pressed === what;
  const held = (what: "publish" | "later" | "now") => changing && pressed !== what;
  return (
    <>
      <SettingsGroup
        id="ci.oidc"
        title="OIDC"
        about="Jobs and deployments prove who they are with tokens from kiln's issuer, which clouds and services trust instead of a stored key."
        words="oidc issuer discovery jwks keys rotate publish subject"
        actions={
          <>
            <Button size="xs" variant="ghost" pending={on("publish")} disabled={held("publish")} onClick={() => keys("publish")}>
              Publish
            </Button>
            <Button size="xs" variant="secondary" aria-expanded={now !== null} onClick={() => setNow(now === null ? ++openings.current : null)}>
              Rotate
            </Button>
          </>
        }
      >
        <Facts
          items={[
            { label: "Issuer", value: <span className="st-value">{d.issuer}</span> },
            { label: "Served here", value: d.serving ? "Yes" : "No: publish its documents where clouds reach them", tone: d.serving ? undefined : "pending" },
            ...(reach
              ? [
                  {
                    label: "From the internet",
                    value: reach.public ? "Its URL is public" : "No: its URL is this network's",
                    tone: reach.public ? undefined : ("pending" as const),
                  },
                ]
              : []),
            ...(reach?.listen.length ? [{ label: "Its own address", value: <span className="st-value">{reach.listen.join(", ")}</span> }] : []),
            ...(reach?.publish
              ? [
                  {
                    label: "Published",
                    value: (
                      <span className="st-value">{`${reach.publish.to}${reach.publish.error ? `: ${reach.publish.error}` : reach.publish.at ? `, ${relativeTime(reach.publish.at)}` : ", not yet"}`}</span>
                    ),
                    tone: reach.publish.error ? ("error" as const) : undefined,
                  },
                ]
              : []),
            { label: "Subject", value: <span className="st-value">{d.subject_claims.length ? d.subject_claims.join(", ") : "GitHub's default"}</span> },
          ]}
        />
        {reach && !reach.public && (
          <p className="st-note">
            AWS and Azure fetch kiln&rsquo;s keys from the issuer: serve it on an address of its own ([ci.oidc] listen) or publish it ([ci.oidc] publish_to), and make that the issuer.
          </p>
        )}
        {now !== null && (
          <Confirm
            className="mt-4"
            label="Rotate the signing key"
            actions={
              <>
                <Button size="xs" variant="primary" pending={on("later")} disabled={held("later")} onClick={() => keys("later")}>
                  In 7 days
                </Button>
                <Button size="xs" variant="danger" pending={on("now")} disabled={held("now")} onClick={() => keys("now")}>
                  Now
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setNow(null)}>
                  Cancel
                </Button>
              </>
            }
          >
            <p>
              A new key is published at once; in 7 days it signs, which gives clouds that cache kiln&rsquo;s keys time to fetch it. Now is for a key you believe leaked: until they fetch the new one,
              its tokens are refused.
            </p>
          </Confirm>
        )}
        <ul className="st-list mt-4" aria-label="Signing keys">
          {d.keys.map((k) => (
            <li key={k.kid}>
              <div className="st-item">
                <span className="st-item-name font-mono text-[12px]">{k.kid}</span>
                <span className="st-item-meta">
                  {k.signing ? "signing now" : k.sign_from > Date.now() / 1000 ? `signs ${relativeTime(k.sign_from)}` : "retired"} · {k.published ? "published" : "not published"} · made{" "}
                  {relativeTime(k.created_at)}
                </span>
              </div>
            </li>
          ))}
        </ul>
        <p className="st-note">
          <a className="text-link" href="/api/ci/oidc/jwks-export" download="kiln-oidc.json">
            The discovery document and key set
          </a>
          , to publish where clouds reach them (kiln oidc publish --to writes them to a directory, an S3 bucket or a Cloud Storage one; kiln oidc jwks prints the keys to paste).
        </p>
        {said && (
          <p className="st-note" data-tone={said.tone} role="status">
            {said.text}
          </p>
        )}
      </SettingsGroup>
      <SettingsGroup
        id="ci.oidc.providers"
        title="Federation"
        about="The integrations that trust kiln's tokens, and whether each did when last verified."
        words="federation aws gcp azure readiness verify trust"
      >
        {federated.length === 0 ? (
          <p className="st-note">None yet: add a cloud account under Deployments, then Verify it.</p>
        ) : (
          <ul className="st-list" aria-label="Federating integrations">
            {federated.map((i) => (
              <li key={i.id}>
                <div className="st-item">
                  <i className="dot" data-status={i.last_error ? "failed" : i.verified_at ? "succeeded" : "pending"} aria-hidden="true" />
                  <span className="st-item-name">{i.name}</span>
                  <span className="st-item-meta">
                    {i.kind} · {NEEDS[i.kind] ?? "trusts kiln's tokens"} ·{" "}
                    {i.last_error
                      ? `refused: ${i.last_error}`
                      : i.verified_at
                        ? `trusted kiln ${relativeTime(i.verified_at)}`
                        : NEEDS[i.kind] === FETCHES && reach && !reach.public
                          ? "not verified yet, and the issuer isn't on the internet"
                          : "not verified yet"}
                  </span>
                </div>
              </li>
            ))}
          </ul>
        )}
      </SettingsGroup>
    </>
  );
}
