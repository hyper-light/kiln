// refused at 5
import { job, step } from "@kiln/sdk";

export default job({
  runs_on: "linux/amd64",
  steps: [step({ run: "true" })],
});
