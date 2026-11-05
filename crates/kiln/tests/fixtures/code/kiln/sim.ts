// kiln's own .kiln/sim.yaml in TypeScript (docs/ci.md W29-AC25), written by translate.mjs.
import { workflow } from "@kiln/sdk";

export default workflow({
  "name": "sim",
  "on": {
    "schedule": [
      {
        "cron": "23 3 * * *"
      }
    ],
    "manual": {}
  },
  "jobs": {
    "nightly": {
      "name": "kiln-sim, nightly seeds",
      "runs-on": [
        "k8s"
      ],
      "toolchain": "rust",
      "resources": {
        "cpu": "4",
        "memory": "16Gi"
      },
      "env": {
        "CARGO_BUILD_JOBS": "4",
        "CARGO_PROFILE_RELEASE_DEBUG": "line-tables-only"
      },
      "timeout-minutes": 240,
      "steps": [
        {
          "name": "build (release)",
          "run": "cargo test --release --locked -p kiln --lib --no-run"
        },
        {
          "name": "sync, 20,000 seeds",
          "run": "KILN_SIM_FROM=$(( $(date -u +%s) / 86400 * 1000000 )) KILN_SIM_SEEDS=20000 cargo test --release --locked -p kiln --lib sim::sync::tests::sync_converges_and_loses_nothing_under_the_nemesis -- --exact --nocapture"
        },
        {
          "name": "moves, 20,000 seeds",
          "run": "KILN_SIM_FROM=$(( $(date -u +%s) / 86400 * 1000000 )) KILN_SIM_SEEDS=20000 cargo test --release --locked -p kiln --lib sim::moves::tests::a_moving_session_never_leaves_its_work_behind -- --exact --nocapture"
        },
        {
          "name": "transfers, 20,000 seeds",
          "run": "KILN_SIM_FROM=$(( $(date -u +%s) / 86400 * 1000000 )) KILN_SIM_SEEDS=20000 cargo test --release --locked -p kiln --lib sim::transfers::tests::w27_ac05_no_stale_act_is_ever_applied -- --exact --nocapture"
        },
        {
          "name": "the goal driver, 2,000 seeds",
          "run": "KILN_SIM_FROM=$(( $(date -u +%s) / 86400 * 1000000 )) KILN_SIM_SEEDS=2000 cargo test --release --locked -p kiln --lib goals::sim::tests::goal_stacks_run_in_order_to_their_end_under_the_nemesis -- --exact --nocapture"
        }
      ]
    }
  }
});
