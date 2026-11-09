// kiln's own .kiln/bench.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "bench",
  "on": {
    "push": {
      "branches": [
        "main"
      ],
      "paths": [
        "crates/**"
      ]
    },
    "manual": {}
  },
  "concurrency": {
    "group": "bench-${{ event }}-${{ branch }}",
    "cancel-in-progress": false
  },
  "jobs": {
    "quic-transport": {
      "name": "QUIC transport bench (hardware)",
      "runs-on": [
        "baremetal"
      ],
      "requires": [
        "os == linux",
        "tools has cargo",
        "tools has python3"
      ],
      "exclusive": true,
      "resources": {
        "cpu": "16",
        "memory": "48Gi"
      },
      "timeout-minutes": 30,
      "steps": [
        {
          "name": "build (release)",
          "run": "cargo test --release --locked -p kiln --no-run"
        },
        {
          "name": "hyper-transport agent⇄server round trips",
          "run": "for i in 1 2 3 4 5; do\n  start=$(date +%s.%N)\n  cargo test --release --locked -p kiln net::tests -- --quiet 2>&1 | grep -E \"test result\"\n  python3 -c \"import sys,time; print(f\\\"run $i: {time.time()-float(sys.argv[1]):.3f} s\\\")\" \"$start\"\ndone\n"
        },
        {
          "name": "hardware",
          "run": "nproc && grep -m1 \"model name\" /proc/cpuinfo && free -h | head -2"
        }
      ]
    },
    "ci-bench": {
      "name": "CI engine targets (hardware)",
      "runs-on": [
        "baremetal"
      ],
      "requires": [
        "os in [linux, macos]",
        "tools has cargo"
      ],
      "exclusive": true,
      "resources": {
        "cpu": "8",
        "memory": "16Gi"
      },
      "timeout-minutes": 60,
      "bench-gate": {
        "metric": "ci.bench.*",
        "max-regression": "10%",
        "same-fingerprint": true,
        "action": "fail"
      },
      "steps": [
        {
          "name": "build (release)",
          "run": "export PATH=\"$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\" && cargo build --release --locked -p kiln"
        },
        {
          "name": "kiln ci bench",
          "run": "target/release/kiln ci bench --out target/ci-bench.json"
        }
      ]
    },
    "noise": {
      "runs-on": [
        "baremetal",
        "bench"
      ],
      "steps": [
        {
          "run": "echo \"independent work, beside the bench on the rest of its machine\"; sleep 5"
        }
      ]
    }
  }
});
