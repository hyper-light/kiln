/** An opened repository's sections (W26-R15, W26-R23), loaded on row intent or opening. The
 * caller retains the existing single review boundary, commits slot and Files focus behavior. */
import type { ShipCommit } from "@/api/ship";
import { Loadable, Loading } from "@/components/ui/loading";
import { SectionHead } from "./head";
import { JobsPane } from "./jobs";
import { ShipOutput } from "./output";
import { ReviewFiles, ShipLanes } from "./review";

export function ShipSections({ commit }: { commit: ShipCommit }) {
  return (
    <>
      <ReviewFiles item={commit} />
      <section aria-label="Jobs" className="ship-sec">
        <JobsPane
          commit={commit}
          output={(picked) => (
            <Loadable label="Loading the output" className="ship-output-slot">
              <ShipOutput commit={commit} picked={picked} />
            </Loadable>
          )}
        />
      </section>
      <section aria-label="Lanes" className="ship-sec">
        <Loadable
          label="Loading the lanes"
          fallback={
            <>
              <SectionHead label="Lanes" />
              <Loading label="Loading the lanes" block className="ship-lanes-slot" />
            </>
          }
        >
          <ShipLanes commit={commit} />
        </Loadable>
      </section>
    </>
  );
}
