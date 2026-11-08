// kiln's own .kiln/wire-lab.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "wire-lab",
  "on": {
    "schedule": [
      {
        "cron": "17 3 * * *"
      }
    ],
    "manual": {}
  },
  "jobs": {
    "seeds": {
      "name": "wire lab, seed ${{ matrix.seed }}",
      "runs-on": [
        "baremetal",
        "linux"
      ],
      "timeout-minutes": 60,
      "matrix": {
        "seed": [
          2,
          3,
          4,
          5,
          6,
          7,
          8,
          9
        ]
      },
      "steps": [
        {
          "name": "the lab (release)",
          "run": "WIRE_LAB_SEED=${{ matrix.seed }} cargo test --release --locked -p kiln --features chaos --test wire_lab -- --nocapture"
        }
      ]
    },
    "garbled": {
      "name": "wire lab, seed ${{ matrix.seed }}, frames made unreadable",
      "runs-on": [
        "baremetal",
        "linux"
      ],
      "timeout-minutes": 60,
      "matrix": {
        "seed": [
          2,
          3,
          4,
          5,
          6,
          7,
          8,
          9
        ]
      },
      "steps": [
        {
          "name": "the lab (release), one frame in 64 unknown and as many garbled",
          "run": "WIRE_LAB_SEED=${{ matrix.seed }} WIRE_LAB_GARBLE=64 cargo test --release --locked -p kiln --features chaos --test wire_lab -- --nocapture"
        }
      ]
    }
  }
});
