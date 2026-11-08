// kiln's own .kiln/e2e.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "e2e",
  "on": {
    "push": {
      "branches": [
        "e2e/*"
      ]
    }
  },
  "jobs": {
    "rust": {
      "name": "rust build + test (pod)",
      "runs-on": [
        "k8s"
      ],
      "toolchain": "rust",
      "resources": {
        "cpu": "2",
        "memory": "2Gi"
      },
      "steps": [
        {
          "name": "toolchain",
          "run": "cargo --version && rustc --version"
        },
        {
          "name": "create a crate",
          "run": "cd /tmp && cargo new --lib --vcs none e2e_demo\nprintf '\\npub fn fib(n: u64) -> u64 {\\n    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }\\n}\\n\\n#[test]\\nfn fib_20() {\\n    assert_eq!(fib(20), 6765);\\n}\\n' >> /tmp/e2e_demo/src/lib.rs\n"
        },
        {
          "name": "build + test",
          "run": "cd /tmp/e2e_demo && cargo build --offline && cargo test --offline"
        }
      ]
    },
    "go": {
      "name": "go vet + test (pod)",
      "runs-on": [
        "k8s"
      ],
      "toolchain": "go",
      "resources": {
        "cpu": "1",
        "memory": "1Gi"
      },
      "steps": [
        {
          "name": "toolchain",
          "run": "go version"
        },
        {
          "name": "create a module",
          "run": "mkdir -p e2e && cd e2e && go mod init example.com/e2e\nprintf 'package e2e\\n\\nfunc Sum(xs ...int) (t int) {\\n\\tfor _, x := range xs {\\n\\t\\tt += x\\n\\t}\\n\\treturn t\\n}\\n' > sum.go\nprintf 'package e2e\\n\\nimport \"testing\"\\n\\nfunc TestSum(t *testing.T) {\\n\\tif Sum(1, 2, 3) != 6 {\\n\\t\\tt.Fatal(\"sum\")\\n\\t}\\n}\\n' > sum_test.go\n"
        },
        {
          "name": "vet + test",
          "run": "cd e2e && go vet ./... && go test -v ./..."
        }
      ]
    },
    "python": {
      "name": "python tests (pod)",
      "runs-on": [
        "k8s"
      ],
      "toolchain": "python",
      "resources": {
        "cpu": "1",
        "memory": "1Gi"
      },
      "steps": [
        {
          "name": "toolchain",
          "run": "python3 --version && uv --version"
        },
        {
          "name": "write a module and its tests",
          "run": "mkdir -p e2e\nprintf 'def mean(xs):\\n    return sum(xs) / len(xs)\\n' > e2e/stats.py\nprintf 'import unittest\\nfrom stats import mean\\n\\n\\nclass T(unittest.TestCase):\\n    def test_mean(self):\\n        self.assertEqual(mean([1, 2, 3]), 2)\\n' > e2e/test_stats.py\n"
        },
        {
          "name": "test",
          "run": "cd e2e && python3 -m unittest -v"
        }
      ]
    },
    "typescript": {
      "name": "typescript test (pod)",
      "runs-on": [
        "k8s"
      ],
      "toolchain": "node",
      "resources": {
        "cpu": "1",
        "memory": "1Gi"
      },
      "steps": [
        {
          "name": "toolchain",
          "run": "node --version && npm --version"
        },
        {
          "name": "write a module and its test",
          "run": "mkdir -p e2e\nprintf 'export function add(a: number, b: number): number {\\n  return a + b;\\n}\\n' > e2e/add.ts\nprintf 'import { test } from \"node:test\";\\nimport assert from \"node:assert\";\\nimport { add } from \"./add.ts\";\\n\\ntest(\"add\", () => assert.equal(add(2, 3), 5));\\n' > e2e/add.test.ts\n"
        },
        {
          "name": "test (node runs TypeScript itself)",
          "run": "cd e2e && node --test add.test.ts"
        }
      ]
    },
    "hardware": {
      "name": "on the bench machine's hardware",
      "runs-on": [
        "baremetal",
        "bench"
      ],
      "steps": [
        {
          "name": "machine",
          "run": "hostname && nproc && grep -m1 \"model name\" /proc/cpuinfo && free -h | head -2"
        },
        {
          "name": "host toolchains",
          "run": "cargo --version\ngo version\npython3 --version\nnode --version\nerl -noshell -eval 'io:format(\"erlang/OTP ~s~n\", [erlang:system_info(otp_release)]), halt().'\n"
        },
        {
          "name": "micro-benchmark",
          "run": "python3 -c \"import timeit; n = 200000; t = timeit.timeit('sum(range(100))', number=n); print(f'sum(range(100)): {t / n * 1e9:.0f} ns per call over {n} calls')\"\n"
        }
      ]
    },
    "fanout-pods": {
      "runs-on": [
        "k8s"
      ],
      "image": "docker.io/library/alpine:3.24",
      "resources": {
        "cpu": "100m",
        "memory": "64Mi"
      },
      "matrix": {
        "shard": [
          0,
          1,
          2,
          3,
          4,
          5,
          6,
          7,
          8,
          9,
          10,
          11,
          12,
          13,
          14,
          15,
          16,
          17,
          18,
          19,
          20,
          21,
          22,
          23,
          24,
          25,
          26,
          27,
          28,
          29,
          30,
          31,
          32,
          33,
          34,
          35,
          36,
          37,
          38,
          39
        ]
      },
      "steps": [
        {
          "run": "echo \"pod shard ${{ matrix.shard }} on $(hostname)\"; sleep 20"
        }
      ]
    },
    "fanout-metal": {
      "runs-on": [
        "baremetal",
        "bench"
      ],
      "matrix": {
        "shard": [
          0,
          1,
          2,
          3,
          4,
          5,
          6,
          7,
          8,
          9,
          10,
          11,
          12,
          13,
          14,
          15
        ]
      },
      "steps": [
        {
          "run": "echo \"metal shard ${{ matrix.shard }} on $KILN_AGENT\"; sleep 10"
        }
      ]
    },
    "gate": {
      "name": "gate (needs every job)",
      "needs": [
        "rust",
        "go",
        "python",
        "typescript",
        "hardware",
        "fanout-pods",
        "fanout-metal"
      ],
      "runs-on": [
        "k8s"
      ],
      "image": "docker.io/library/alpine:3.24",
      "resources": {
        "cpu": "100m",
        "memory": "64Mi"
      },
      "steps": [
        {
          "run": "echo \"every job passed\""
        }
      ]
    }
  }
});
