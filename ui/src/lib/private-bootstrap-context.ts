import { createContext } from "react";
import type { PrivateBootstrap } from "./private-bootstrap";

/** Per-tree loader value; server reads never touch the browser's application stores. */
export const PrivateBootstrapContext = createContext<PrivateBootstrap | null>(null);
