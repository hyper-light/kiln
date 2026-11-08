// kiln's own .kiln/custody-lab.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "custody-lab",
  "on": {
    "schedule": [
      {
        "cron": "41 3 * * *"
      }
    ],
    "manual": {}
  },
  "jobs": {
    "custody": {
      "name": "custody's hand-overs (release, faults)",
      "runs-on": [
        "baremetal",
        "linux"
      ],
      "timeout-minutes": 60,
      "steps": [
        {
          "name": "transfers.rs and custody.rs (release, chaos)",
          "run": "cargo test --release --locked -p kiln --features chaos --test transfers --test custody -- --nocapture"
        }
      ]
    }
  }
});
