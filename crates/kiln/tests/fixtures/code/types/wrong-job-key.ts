// refused at 7
import { workflow, job } from "@kiln/sdk";

export default workflow({
  on: "push",
  jobs: {
    build: job({ "runs-on": "linux/amd64", strategyy: { "fail-fast": false } }),
  },
});
