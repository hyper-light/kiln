/**
 * The browser's entry: watch for new builds, then hydrate what the server rendered.
 */
import { StrictMode } from "react";
import { hydrateRoot } from "react-dom/client";
import { StartClient } from "@tanstack/react-start/client";
import { watchForUpdates } from "./lib/update";

watchForUpdates();

hydrateRoot(
  document,
  <StrictMode>
    <StartClient />
  </StrictMode>,
);
