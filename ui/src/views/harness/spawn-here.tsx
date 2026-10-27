import { Bot } from "lucide-react";
import { Tip } from "@/components/ui/tooltip";
import { useNavigate } from "@/lib/router";
import { startFrom } from "./new-agent-draft";

/** Start agents in this shell: Mission control's composer, filled in with the shell (its machine and folder). */
export function SpawnHereButton({ target, shell }: { target: string; shell: string | null }) {
  const navigate = useNavigate();
  return (
    <Tip content="Start agents in this shell">
      <button
        className="icon-btn"
        data-variant="bare"
        data-size="sm"
        aria-label="Start agents in this shell"
        disabled={!shell}
        onClick={() => shell && startFrom(navigate, { from: "From Terminal: it starts in this shell", target, shell })}
      >
        <Bot size={14} />
      </button>
    </Tip>
  );
}
