/**
 * Ship's three views (plan 01a175b7f20b7277b15bddd2ad001bcc, step 1): its commits, and the runs and
 * deployments it took the place of in the nav, each keeping its address and its filters.
 */
import { NavLink } from "@/lib/router";
import "./ship.css";

const VIEWS = [
  { to: "/ship", label: "Commits" },
  { to: "/runs", label: "Runs" },
  { to: "/deployments", label: "Deployments" },
] as const;

export function ShipViews() {
  return (
    <nav className="ship-views" aria-label="Ship views">
      {VIEWS.map((v) => (
        <NavLink key={v.to} to={v.to} className={({ isActive }) => (isActive ? "ship-views__link is-current" : "ship-views__link")}>
          {v.label}
        </NavLink>
      ))}
    </nav>
  );
}
