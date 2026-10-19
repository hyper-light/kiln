/**
 * The document every page is rendered in: its head (meta, styles, fonts) and the scripts. The
 * server renders it; pages render inside.
 */
import * as React from "react";
import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
import appCss from "@/index.css?url";
import geist from "@fontsource-variable/geist?url";
import geistMono from "@fontsource-variable/geist-mono?url";

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

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1, viewport-fit=cover" },
      { name: "theme-color", content: "#08090a" },
      { name: "color-scheme", content: "dark" },
      { name: "description", content: "kiln — self-hosted CI/CD for the hyper-light stack" },
      // Added to a phone's Home Screen, kiln opens as an app of its own (iPhone: what Web Push needs).
      { name: "apple-mobile-web-app-capable", content: "yes" },
      { name: "apple-mobile-web-app-title", content: "kiln" },
      { title: "kiln" },
    ],
    links: [
      { rel: "stylesheet", href: geist },
      { rel: "stylesheet", href: geistMono },
      { rel: "stylesheet", href: appCss },
      { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" },
      { rel: "manifest", href: "/manifest.webmanifest" },
      { rel: "apple-touch-icon", href: "/apple-touch-icon.png" },
    ],
  }),
  shellComponent: Document,
  component: Outlet,
});

function Document({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" data-motion="playing">
      <head>
        <HeadContent />
        <script dangerouslySetInnerHTML={{ __html: NO_AUTO_ZOOM }} />
        <style>{"html,body{background:#08090a;color:#ededee;margin:0}"}</style>
      </head>
      <body>
        {children}
        <Scripts />
      </body>
    </html>
  );
}
