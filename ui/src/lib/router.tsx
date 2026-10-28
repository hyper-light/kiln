/**
 * The React Router APIs kiln's pages were written with (Link, NavLink, Navigate, Outlet,
 * useNavigate, useParams, useSearchParams, useLocation), on TanStack Router, so the pages moved
 * into its routes unchanged. Paths are plain strings ("/runs/1?x=2#y"), and search params are
 * strings, as the router keeps them (router.tsx reads and writes them as URLSearchParams). New code
 * uses TanStack Router's own, typed APIs instead.
 */
import * as React from "react";
import { Link as TanLink, Navigate as TanNavigate, Outlet, useLocation as useTanLocation, useNavigate as useTanNavigate, useParams as useTanParams, useRouter } from "@tanstack/react-router";

export { Outlet };

/** A path's parts: "/runs/1?x=2#y" → its pathname, search params and hash. */
export function split(to: string): { pathname: string; search: Record<string, string>; hash: string } {
  const url = new URL(to, "http://kiln.invalid");
  return { pathname: url.pathname, search: Object.fromEntries(url.searchParams), hash: url.hash.replace(/^#/, "") };
}

/** TanStack's Link and Navigate, without the route typing (these take any path). */
const AnyLink = TanLink as unknown as React.ComponentType<Record<string, unknown>>;
const AnyNavigate = TanNavigate as unknown as React.ComponentType<Record<string, unknown>>;

type AnchorProps = Omit<React.AnchorHTMLAttributes<HTMLAnchorElement>, "href" | "className">;

type LinkProps = AnchorProps & {
  to: string;
  replace?: boolean;
  className?: string;
  /** When it counts as the current page (TanStack's own: it then sets `aria-current="page"`). */
  activeOptions?: { exact?: boolean; includeSearch?: boolean };
};

export const Link = React.forwardRef<HTMLAnchorElement, LinkProps>(function Link({ to, replace, ...rest }, ref) {
  const p = split(to);
  return <AnyLink ref={ref} to={p.pathname} search={p.search} hash={p.hash || undefined} replace={replace} {...rest} />;
});

/** A Link that knows whether it's the current page (`end`: only exactly, not below it; `also`:
 * other paths it's the current page for, and below them, as Ship is for runs and deployments). */
export function NavLink({
  to,
  end,
  also,
  className,
  children,
  ...rest
}: AnchorProps & {
  to: string;
  end?: boolean;
  also?: readonly string[];
  className?: string | ((s: { isActive: boolean }) => string | undefined);
  children?: React.ReactNode | ((s: { isActive: boolean }) => React.ReactNode);
}) {
  const { pathname } = useTanLocation();
  const target = split(to).pathname;
  const exact = Boolean(end) || target === "/";
  // The same rule TanStack's Link applies to set aria-current (below the path, or exactly).
  const under = (t: string) => pathname === t || pathname.startsWith(`${t.replace(/\/$/, "")}/`);
  const own = exact ? pathname === target : under(target);
  const isActive = own || (also ?? []).some(under);
  return (
    <Link
      to={to}
      activeOptions={{ exact, includeSearch: false }}
      className={typeof className === "function" ? className({ isActive }) : className}
      {...rest}
      // Below one of `also`, TanStack counts it inactive: it's the current page all the same.
      {...(isActive && !own ? { "aria-current": "page" as const } : {})}
    >
      {typeof children === "function" ? children({ isActive }) : children}
    </Link>
  );
}

export function Navigate({ to, replace }: { to: string; replace?: boolean }) {
  const p = split(to);
  return <AnyNavigate to={p.pathname} search={p.search} hash={p.hash || undefined} replace={replace} />;
}

/** navigate("/runs/1?x=2", { replace }) or navigate(-1). */
export function useNavigate(): (to: string | number, opts?: { replace?: boolean; viewTransition?: boolean }) => void {
  const navigate = useTanNavigate();
  const router = useRouter();
  return React.useCallback(
    (to: string | number, opts?: { replace?: boolean; viewTransition?: boolean }) => {
      if (typeof to === "number") {
        router.history.go(to);
        return;
      }
      const p = split(to);
      // A view transition only while motion plays (kiln's own pause, or the device's reduced motion: motion.tsx).
      const viewTransition = opts?.viewTransition && document.documentElement.dataset.motion !== "paused" ? true : undefined;
      void navigate({ to: p.pathname, search: p.search, hash: p.hash || undefined, replace: opts?.replace, viewTransition } as never);
    },
    [navigate, router],
  );
}

/** The current route's params ("/runs/$id" → { id }), whatever the route. */
export function useParams(): Record<string, string | undefined> {
  const params: unknown = useTanParams({ strict: false });
  return params as Record<string, string | undefined>;
}

/** Where the page is. `key` is new for every navigation (as React Router's was), from the history entry. */
export function useLocation(): { pathname: string; search: string; hash: string; state: unknown; key: string } {
  const l = useTanLocation();
  const search = l.searchStr && !l.searchStr.startsWith("?") ? `?${l.searchStr}` : (l.searchStr ?? "");
  const state = l.state as { __TSR_key?: string; key?: string } | undefined;
  return { pathname: l.pathname, search, hash: l.hash ? `#${l.hash}` : "", state: l.state, key: state?.__TSR_key ?? state?.key ?? "default" };
}

type SetSearch = (next: URLSearchParams | Record<string, string> | ((prev: URLSearchParams) => URLSearchParams | Record<string, string>), opts?: { replace?: boolean }) => void;

export function useSearchParams(): [URLSearchParams, SetSearch] {
  const { pathname, searchStr } = useTanLocation();
  const navigate = useTanNavigate();
  const params = React.useMemo(() => new URLSearchParams(searchStr), [searchStr]);
  const set = React.useCallback<SetSearch>(
    (next, opts) => {
      const value = typeof next === "function" ? next(new URLSearchParams(searchStr)) : next;
      const search = Object.fromEntries(value instanceof URLSearchParams ? value : new URLSearchParams(value));
      void navigate({ to: pathname, search, replace: opts?.replace } as never);
    },
    [navigate, pathname, searchStr],
  );
  return [params, set];
}
