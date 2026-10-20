/**
 * What shows while something loads: hyperlight's spectrum rule, a short prism line that drifts.
 * It appears only after a moment, so a fast load shows nothing, and it holds still under reduced
 * motion. `Loadable` puts it behind a Suspense boundary that also says, in place, when loading
 * failed, with a way to try again.
 */
import * as React from "react";
import { errorMessage } from "@/lib/hooks";
import { retryFailed, useLanded } from "@/lib/resource";
import { cn } from "@/lib/utils";

/** The loader. `block`: it stands in for a section, so it keeps some height. */
export function Loading({ label = "Loading", block, className }: { label?: string; block?: boolean; className?: string }) {
  return (
    <div className={cn("hl-loading", block && "hl-loading-block", className)} role="status">
      <span className="hl-rule" aria-hidden="true" />
      <span className="sr-only">{label}</span>
    </div>
  );
}

/**
 * A section that loads its data with `useSuspenseResource`: the loader until the first data is
 * in, then the data, refreshed in place from then on. `fallback`: a stand-in shaped like the
 * section, so nothing moves when it arrives.
 */
export function Loadable({ children, label, fallback, className }: { children: React.ReactNode; label?: string; fallback?: React.ReactNode; className?: string }) {
  // Rendered again as data lands, so what waited on it shows at once (see `useLanded`).
  useLanded();
  return (
    <LoadBoundary label={label}>
      <React.Suspense fallback={fallback ?? <Loading label={label} block className={className} />}>{children}</React.Suspense>
    </LoadBoundary>
  );
}

class LoadBoundary extends React.Component<{ label?: string; children: React.ReactNode }, { failed: boolean; error: unknown }> {
  state = { failed: false, error: undefined as unknown };

  static getDerivedStateFromError(error: unknown) {
    return { failed: true, error };
  }

  render() {
    if (!this.state.failed) return this.props.children;
    const what = this.props.label ? this.props.label.replace(/^Loading /i, "") : "this";
    return (
      <div className="hl-failed" role="alert">
        <span>
          Couldn&apos;t load {what}: {errorMessage(this.state.error)}
        </span>
        <button
          type="button"
          className="hl-retry"
          onClick={() => {
            retryFailed();
            this.setState({ failed: false, error: undefined });
          }}
        >
          Try again
        </button>
      </div>
    );
  }
}
