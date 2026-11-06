/**
 * The first moment, while the app asks who this is: kiln's mark and the loader. In the page's HTML
 * too (routes/__root.tsx), so a slow link shows it with the first few KB instead of after all the
 * code (goal 8, plan 01a1a832ebfd step 7); the app's own takes its place, looking the same.
 */
import { BrandMark } from "@/components/shell/brand";
import { Loading } from "@/components/ui/loading";

export function Splash() {
  return (
    <div className="grid min-h-dvh place-items-center" aria-busy="true">
      <div className="flex flex-col items-center gap-5 text-[#ededee]">
        <BrandMark className="h-9 w-8 opacity-90" />
        <Loading label="Loading kiln" />
      </div>
    </div>
  );
}
