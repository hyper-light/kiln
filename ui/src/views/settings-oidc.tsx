/**
 * Settings → CI → OIDC (docs/ci.md W21-R11, W10-R06, R12): kiln's issuer (where it is, and whether
 * this server serves its documents), its signing keys (which signs, which are published), rotating
 * and publishing them, the key set to publish elsewhere, and each federating integration's
 * readiness (its last Verify, and what went wrong).
 */
import * as React from "react";
import { api, type Integrations } from "@/api/client";
import { ci, type CiOidc } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Confirm, Facts } from "@/components/ui/misc";
import { SettingsGroup } from "@/components/ui/settings";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";

/** Kinds that trust kiln's tokens instead of a stored secret. */
const FEDERATING = new Set(["aws", "gcp", "azure"]);

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
  const integrations: Integrations = useSuspenseResource("ci/integrations", (s) => api.integrations(s)).data;
  const federated = integrations.integrations.filter((i) => FEDERATING.has(i.kind) || (i.kind === "oci" && typeof i.config.federation === "string"));
  const [busy, setBusy] = React.useState(false);
  const [said, setSaid] = React.useState<{ tone?: "error"; text: string } | null>(null);
  const [now, setNow] = React.useState(false);
  const act = async (f: () => Promise<string>) => {
    setBusy(true);
    setSaid(null);
    try {
      setSaid({ text: await f() });
      res.reload();
    } catch (e) {
      setSaid({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <SettingsGroup
        id="ci.oidc"
        title="OIDC"
        about="Jobs and deployments prove who they are with tokens from kiln's issuer, which clouds and services trust instead of a stored key."
        words="oidc issuer discovery jwks keys rotate publish subject"
        actions={
          <>
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act(async () => `Published to ${(await ci.oidcPublish()).published}.`)}>
              Publish
            </Button>
            <Button size="xs" variant="secondary" disabled={busy} aria-expanded={now} onClick={() => setNow(!now)}>
              Rotate
            </Button>
          </>
        }
      >
        <Facts
          items={[
            { label: "Issuer", value: <span className="st-value">{d.issuer}</span> },
            { label: "Served here", value: d.serving ? "Yes" : "No: publish its documents where clouds reach them", tone: d.serving ? undefined : "pending" },
            { label: "Subject", value: <span className="st-value">{d.subject_claims.length ? d.subject_claims.join(", ") : "GitHub's default"}</span> },
          ]}
        />
        {now && (
          <Confirm
            className="mt-4"
            label="Rotate the signing key"
            actions={
              <>
                <Button
                  size="xs"
                  variant="primary"
                  disabled={busy}
                  onClick={() => void act(async () => `A new key signs from ${new Date((await ci.oidcRotate(false)).sign_from * 1000).toLocaleString()}.`).then(() => setNow(false))}
                >
                  In 7 days
                </Button>
                <Button
                  size="xs"
                  variant="danger"
                  disabled={busy}
                  onClick={() =>
                    void act(async () => {
                      await ci.oidcRotate(true);
                      return "A new key signs now.";
                    }).then(() => setNow(false))
                  }
                >
                  Now
                </Button>
                <Button size="xs" variant="ghost" disabled={busy} onClick={() => setNow(false)}>
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
          , to publish where clouds reach them (kiln oidc publish --to writes them to a directory).
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
                    {i.kind} · {i.last_error ? `refused: ${i.last_error}` : i.verified_at ? `trusted kiln ${relativeTime(i.verified_at)}` : "not verified yet"}
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
