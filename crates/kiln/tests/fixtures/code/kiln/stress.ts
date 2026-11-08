// kiln's own .kiln/stress.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "stress",
  "on": {
    "manual": {}
  },
  "jobs": {
    "pods": {
      "runs-on": [
        "k8s"
      ],
      "image": "docker.io/library/alpine:3.24",
      "resources": {
        "cpu": "100m",
        "memory": "64Mi"
      },
      "matrix": {
        "a": [
          0,
          1,
          2,
          3,
          4,
          5,
          6,
          7
        ],
        "b": [
          0,
          1,
          2,
          3,
          4,
          5
        ]
      },
      "steps": [
        {
          "run": "echo \"pod leg ${{ matrix.a }}-${{ matrix.b }} on $(hostname)\"; test -f .kiln/stress.yaml; sleep 2"
        }
      ]
    },
    "metal": {
      "runs-on": [
        "baremetal"
      ],
      "matrix": {
        "a": [
          0,
          1,
          2,
          3,
          4,
          5,
          6,
          7
        ],
        "b": [
          0,
          1,
          2,
          3,
          4,
          5,
          6,
          7
        ]
      },
      "steps": [
        {
          "run": "echo \"metal leg ${{ matrix.a }}-${{ matrix.b }} on $KILN_AGENT\"; sleep 1"
        }
      ]
    },
    "gate": {
      "needs": [
        "pods",
        "metal"
      ],
      "runs-on": [
        "baremetal"
      ],
      "steps": [
        {
          "run": "echo \"all 112 legs passed\""
        }
      ]
    }
  }
});
