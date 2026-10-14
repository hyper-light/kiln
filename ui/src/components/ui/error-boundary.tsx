import * as React from "react";
import { RotateCcw } from "lucide-react";
import { Button } from "./button";

interface Props {
  children: React.ReactNode;
  /** When this changes, the boundary resets (e.g. route path). */
  resetKey?: unknown;
  label?: string;
  compact?: boolean;
}
interface State {
  error: Error | null;
}

/** Keeps the console from ever white-screening; offers an in-place retry. */
export class ErrorBoundary extends React.Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error("[kiln-ui] render error", error, info.componentStack);
  }

  componentDidUpdate(prev: Props) {
    if (this.state.error && prev.resetKey !== this.props.resetKey) this.setState({ error: null });
  }

  render() {
    if (!this.state.error) return this.props.children;
    const isChunk = /dynamically imported module|Loading chunk|Failed to fetch/i.test(this.state.error.message);
    return (
      <div
        role="alert"
        className={
          this.props.compact
            ? "rounded-md border border-dashed border-[#3a3036] p-5 text-center"
            : "container-console py-24 text-center"
        }
      >
        <div className="eyebrow justify-center">
          <span className="spectrum-rule" /> {this.props.label ?? "This view crashed"}
        </div>
        <h2 className="mx-auto mt-4 max-w-xl text-[24px] font-[450] tracking-[-0.03em]">
          {isChunk ? "A newer build is available." : "Something went wrong rendering this view."}
        </h2>
        <p className="mx-auto mt-3 max-w-xl break-words font-mono text-[11px] leading-relaxed text-[#9a9da6]">
          {this.state.error.message}
        </p>
        <div className="mt-6 flex justify-center gap-2">
          <Button variant="secondary" onClick={() => this.setState({ error: null })}>
            <RotateCcw /> Try again
          </Button>
          {isChunk && (
            <Button variant="ghost" onClick={() => location.reload()}>
              Reload
            </Button>
          )}
        </div>
      </div>
    );
  }
}
