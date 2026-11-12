/**
 * A question an agent (or one of its subagents) asks, answered right under its row in Mission
 * control, drawn as the chat draws it (approval.tsx's `Questions`, its head left to the row): its
 * options, Other, Decline and Submit. The question whole comes from the asking session's own state
 * (the row has only its title); answered (answered.ts), the row moves on at once.
 */
import { sessionStore, useSession, useSessionReady, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import { Loadable } from "@/components/ui/loading";
import { Skeleton } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { answer } from "./answered";
import { Questions } from "./approval";

export function AnswerHere({ who, ap }: { who: AgentInfo; ap: ApprovalBrief }) {
  // W21-R02: the waiting row remains subscribed while its first-snapshot reader suspends.
  useSession(sessionStore(who.id), (s) => s.ready);
  return (
    <Loadable key={who.id} fallback={<Skeleton className="ms-answer-wait" />}>
      <Asked who={who} ap={ap} />
    </Loadable>
  );
}

function Asked({ who, ap }: { who: AgentInfo; ap: ApprovalBrief }) {
  const store = sessionStore(who.id);
  useSessionReady(store);
  const full = useSession(store, (s) => s.state?.approvals.find((x) => x.id === ap.id));
  // Answered elsewhere meanwhile: nothing's left to answer here.
  if (!full) return null;
  return <Questions a={full} headless agentName={who.name} kind={who.kind_name} onDecide={(c) => void answer(who.id, c).catch((e: unknown) => toast.error("Couldn't answer", errorMessage(e)))} />;
}
