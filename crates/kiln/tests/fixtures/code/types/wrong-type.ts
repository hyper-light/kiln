// refused at 6
import { job, step } from "@kiln/sdk";

export default job({
  "runs-on": "linux/amd64",
  "timeout-minutes": "thirty",
  steps: [step({ run: "true" })],
});
