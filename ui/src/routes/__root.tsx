/**
 * The document every page is rendered in: its head (meta, styles, fonts) and the scripts. The
 * server renders it; pages render inside.
 */
import * as React from "react";
import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
import appCss from "@/index.css?url";
import geist from "@fontsource-variable/geist?url";
import geistMono from "@fontsource-variable/geist-mono?url";
import { Splash } from "@/components/shell/splash";
import { EARLY_SCRIPT } from "@/lib/early-script";
import geistLatin from "@fontsource-variable/geist/files/geist-latin-wght-normal.woff2?url";
import geistMonoLatin from "@fontsource-variable/geist-mono/files/geist-mono-latin-wght-normal.woff2?url";
import { privatePage } from "@/lib/private-bootstrap";
import { PrivateBootstrapContext } from "@/lib/private-bootstrap-context";
import { getPrivateBootstrap } from "@/server/private-bootstrap";

/**
 * Phone browsers zoom the page into a focused field whose text is under 16px (kiln focuses the
 * terminal on load): every iOS browser (all WebKit), and some Android ones. On touch devices, tell
 * them not to auto-zoom; iOS still lets people pinch to zoom.
 */
const NO_AUTO_ZOOM = `(function () {
  var ios = /iP(hone|ad|od)/.test(navigator.userAgent) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
  var touch = ios || (window.matchMedia && window.matchMedia("(pointer: coarse)").matches);
  if (!touch) return;
  var m = document.querySelector('meta[name="viewport"]');
  if (m) m.setAttribute("content", m.getAttribute("content") + ", maximum-scale=1");
})();`;

const VIEWPORT = "width=device-width, initial-scale=1, viewport-fit=cover";

/**
 * The viewport as NO_AUTO_ZOOM left it (W21-R13). React keeps the page's own tag only while its
 * content is what React renders: otherwise it adds a plain one after it, and phones follow the last
 * tag, so they zoomed into fields again once the page had loaded.
 */
function viewport() {
  return (typeof document !== "undefined" && document.querySelector<HTMLMetaElement>('meta[name="viewport"]')?.content) || VIEWPORT;
}

export const Route = createRootRoute({
  // Only the document request bootstraps identity. Client navigation must never wait on another
  // identity/list read, nor replay private data from an earlier page after logout.
  loader: ({ location }) => (typeof window === "undefined" && privatePage(location.pathname) ? getPrivateBootstrap({ data: location.pathname }) : null),
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: viewport() },
      { name: "theme-color", content: "#08090a" },
      { name: "color-scheme", content: "dark" },
      {
        name: "description",
        content: "kiln — self-hosted CI/CD for the hyper-light stack",
      },
      // Added to a phone's Home Screen, kiln opens as an app of its own (iPhone: what Web Push needs).
      { name: "mobile-web-app-capable", content: "yes" },
      { name: "apple-mobile-web-app-capable", content: "yes" },
      { name: "apple-mobile-web-app-title", content: "kiln" },
      { title: "kiln" },
    ],
    links: [
      {
        rel: "preload",
        href: geistLatin,
        as: "font",
        type: "font/woff2",
        crossOrigin: "anonymous",
      },
      {
        rel: "preload",
        href: geistMonoLatin,
        as: "font",
        type: "font/woff2",
        crossOrigin: "anonymous",
      },
      { rel: "stylesheet", href: geist },
      { rel: "stylesheet", href: geistMono },
      { rel: "stylesheet", href: appCss },
      { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" },
      { rel: "manifest", href: "/manifest.webmanifest" },
      { rel: "apple-touch-icon", href: "/apple-touch-icon.png" },
    ],
  }),
  shellComponent: Document,
  component: Root,
});

function Root() {
  const bootstrap = Route.useLoaderData();
  return (
    <PrivateBootstrapContext value={bootstrap}>
      <Outlet />
    </PrivateBootstrapContext>
  );
}

function Document({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" data-motion="playing">
      <head>
        {/* The page's first reads, asked while its code is on its way (lib/early.ts): the server's to write, not the browser's code to carry. */}
        <script suppressHydrationWarning dangerouslySetInnerHTML={{ __html: import.meta.env.SSR ? EARLY_SCRIPT : "" }} />
        <HeadContent />
        <script dangerouslySetInnerHTML={{ __html: NO_AUTO_ZOOM }} />
        {/* The splash in the HTML stands until the page puts anything of its own after it (goal 8 step 7). */}
        <style>{"html,body{background:#08090a;color:#ededee;margin:0}#kiln-splash:has(~ :not(script,template)){display:none}"}</style>
      </head>
      <body>
        <div id="kiln-splash">
          <Splash />
        </div>
        {children}
        <Scripts />
      </body>
    </html>
  );
}
