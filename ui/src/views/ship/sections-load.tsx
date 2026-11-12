/** W26-R15: one canonical code resource for the sections a Ship row opens. */
import type { ShipCommit } from "@/api/ship";
import { useRes, type Res } from "@/lib/data";

export const sectionsRes: Res<typeof import("./sections")> = { key: "code/ship/sections", fetch: () => import("./sections") };

export function OpenSections({ commit }: { commit: ShipCommit }) {
  const { ShipSections } = useRes(sectionsRes).data;
  return <ShipSections commit={commit} />;
}
