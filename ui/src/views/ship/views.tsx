/**
 * Ship's two views (docs/ci.md W26-R24, as Ada approved them on 2026-10-30): its commits, and its
 * deploys. The runs' list is the commits' now (each commit's jobs act on its runs), and the apps'
 * list is Deploys; a run's and an app's pages keep their addresses.
 */
import { NavLink } from "@/lib/router";
import "./ship.css";

const VIEWS = [
  { to: "/ship", label: "Commits", end: true },
  { to: "/ship/deploys", label: "Deploys", end: false },
] as const;

export function ShipViews() {
  return (
    <nav className="ship-views" aria-label="Ship views">
      {VIEWS.map((v) => (
        <NavLink key={v.to} to={v.to} end={v.end} className={({ isActive }) => (isActive ? "ship-views__link is-current" : "ship-views__link")}>
          {v.label}
        </NavLink>
      ))}
    </nav>
  );
}
