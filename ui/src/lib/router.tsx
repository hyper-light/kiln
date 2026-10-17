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

export const Link = React.forwardRef<HTMLAnchorElement, AnchorProps & { to: string; replace?: boolean; className?: string }>(function Link({ to, replace, ...rest }, ref) {
  const p = split(to);
  return <AnyLink ref={ref} to={p.pathname} search={p.search} hash={p.hash || undefined} replace={replace} {...rest} />;
});

/** A Link that knows whether it's the current page (`end`: only exactly, not below it). */
export function NavLink({
  to,
  end,
  className,
  children,
  ...rest
}: AnchorProps & {
  to: string;
  end?: boolean;
  className?: string | ((s: { isActive: boolean }) => string | undefined);
  children?: React.ReactNode | ((s: { isActive: boolean }) => React.ReactNode);
}) {
  const { pathname } = useTanLocation();
  const target = split(to).pathname;
  const isActive = end || target === "/" ? pathname === target : pathname === target || pathname.startsWith(`${target.replace(/\/$/, "")}/`);
  return (
    <Link to={to} className={typeof className === "function" ? className({ isActive }) : className} aria-current={isActive ? "page" : undefined} {...rest}>
      {typeof children === "function" ? children({ isActive }) : children}
    </Link>
  );
}

export function Navigate({ to, replace }: { to: string; replace?: boolean }) {
  const p = split(to);
  return <AnyNavigate to={p.pathname} search={p.search} hash={p.hash || undefined} replace={replace} />;
}

/** navigate("/runs/1?x=2", { replace }) or navigate(-1). */
export function useNavigate(): (to: string | number, opts?: { replace?: boolean }) => void {
  const navigate = useTanNavigate();
  const router = useRouter();
  return React.useCallback(
    (to: string | number, opts?: { replace?: boolean }) => {
      if (typeof to === "number") {
        router.history.go(to);
        return;
      }
      const p = split(to);
      void navigate({ to: p.pathname, search: p.search, hash: p.hash || undefined, replace: opts?.replace } as never);
    },
    [navigate, router],
  );
}

/** The current route's params ("/runs/$id" → { id }), whatever the route. */
export function useParams(): Record<string, string | undefined> {
  const params: unknown = useTanParams({ strict: false });
  return params as Record<string, string | undefined>;
}

export function useLocation(): { pathname: string; search: string; hash: string; state: unknown } {
  const l = useTanLocation();
  const search = l.searchStr && !l.searchStr.startsWith("?") ? `?${l.searchStr}` : (l.searchStr ?? "");
  return { pathname: l.pathname, search, hash: l.hash ? `#${l.hash}` : "", state: l.state };
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
