# Every record kiln keeps

CP-T1.5 to CP-T1.7 (G9 step 2): what moves to Records on Amphora. Written by
`scripts/records-inventory.sh`; `crates/kiln-seams/tests/records_inventory.rs` fails when this
file and the code differ, and says how it reads the code. A table only tests make, or one
dropped by a later migration, is listed too, and says so.

## SQLite tables

| Table | Created in | Dropped or renamed in | Tests only |
|---|---|---|---|
| `a` | `crates/kiln/src/store/migrations.rs` |  | yes |
| `agent_def_experiment_criteria` | `crates/kiln/src/store/migrations/20261116-2000_agent_def_experiments.sql` |  |  |
| `agent_def_experiment_draws` | `crates/kiln/src/store/migrations/20261116-2000_agent_def_experiments.sql` |  |  |
| `agent_def_experiment_variants` | `crates/kiln/src/store/migrations/20261116-2000_agent_def_experiments.sql` |  |  |
| `agent_def_experiments` | `crates/kiln/src/store/migrations/20261116-2000_agent_def_experiments.sql` |  |  |
| `agent_def_sessions` | `crates/kiln/src/store/migrations/20261115-0100_agent_def_sessions.sql` |  |  |
| `agent_def_uses` | `crates/kiln/src/store/migrations/20261114-2100_agent_def_uses.sql` |  |  |
| `agent_def_versions` | `crates/kiln/src/store/migrations/20261114-2000_agent_defs.sql` |  |  |
| `agent_defs` | `crates/kiln/src/store/migrations/20261114-2000_agent_defs.sql` |  |  |
| `agent_mail` | `crates/kiln/src/store/migrations/20261111-0100_agent_mail.sql` |  |  |
| `agent_mail_handoffs` | `crates/kiln/src/store/migrations/20261111-0100_agent_mail.sql` |  |  |
| `agent_mail_transfers` | `crates/kiln/src/store/migrations/20261111-0100_agent_mail.sql` |  |  |
| `agent_mail_usage` | `crates/kiln/src/store/migrations/20261111-0100_agent_mail.sql` |  |  |
| `agentenv_sessions` | `crates/kiln/src/store/migrations/20261118-1100_agentenvs.sql` |  |  |
| `agentenv_stacks` | `crates/kiln/src/store/migrations/20261119-1200_agentenv_stacks.sql` |  |  |
| `agentenvs` | `crates/kiln/src/store/migrations/20261118-1100_agentenvs.sql` |  |  |
| `agentfile_versions` | `crates/kiln/src/store/migrations/20261111-2000_agentfiles.sql` |  |  |
| `agentfiles` | `crates/kiln/src/store/migrations/20261111-2000_agentfiles.sql` |  |  |
| `agents` | `crates/kiln/src/store.rs` |  | yes |
| `attention_archive` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_archive_groups` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_choice_exposures` | `crates/kiln/src/store/migrations/20261108-0101_w21_attention_examination.sql` |  |  |
| `attention_choices` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_exposures` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_learned` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_meta` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_placement` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_rendered` | `crates/kiln/src/store/migrations/20261108-0101_w21_attention_examination.sql` |  |  |
| `attention_request_heads` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_requests` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_response_attempts` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_responses` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_settings` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_uses` | `crates/kiln/src/store/migrations/20261108-0100_w21_attention.sql` |  |  |
| `attention_visit_feedback` | `crates/kiln/src/store/migrations/20261108-0101_w21_attention_examination.sql` |  |  |
| `attention_visits` | `crates/kiln/src/store/migrations/20261108-0101_w21_attention_examination.sql` |  |  |
| `audit` | `crates/kiln/src/store.rs`, `crates/kiln/src/store/capture.rs` |  | yes |
| `audit2` | `crates/kiln/src/harness/attention/examination.rs`, `crates/kiln/src/harness/attention/learned.rs`, `crates/kiln/src/harness/attention/mod.rs`, `crates/kiln/src/harness/attention/requests.rs`, `crates/kiln/src/harness/attention/store.rs`, `crates/kiln/src/store/migrations/20261018-2200_w19_audit2.sql` |  |  |
| `audit2_indexed` | `crates/kiln/src/store/migrations/20261106-2103_dbq_store.sql` |  |  |
| `audit2_search` | `crates/kiln/src/store/migrations/20261106-2103_dbq_store.sql` |  |  |
| `b` | `crates/kiln/src/store/migrations.rs` |  | yes |
| `baseline` | `crates/kiln/src/store/storage_tests.rs` |  | yes |
| `blob_keys` | `crates/kiln/src/store/migrations/20261021-1200_t10_blobs.sql` |  |  |
| `blob_parts` | `crates/kiln/src/store/migrations/20261021-1200_t10_blobs.sql` |  |  |
| `blob_uploads` | `crates/kiln/src/store/migrations/20261021-1200_t10_blobs.sql` |  |  |
| `broker_ends` | `crates/kiln/src/store/migrations/20261113-1400_cp_t8_broker_ledger.sql` |  |  |
| `broker_leases` | `crates/kiln/src/store/migrations/20261113-1400_cp_t8_broker_ledger.sql` |  |  |
| `broker_runs` | `crates/kiln/src/store/migrations/20261113-1400_cp_t8_broker_ledger.sql` |  |  |
| `cas_chunks` | `crates/kiln/src/store/migrations/20261017-2330_w08_store.sql` |  |  |
| `cas_index` | `crates/kiln/src/store/migrations/20261021-1200_t10_blobs.sql` |  |  |
| `cas_objects` | `crates/kiln/src/store/migrations/20261017-2330_w08_store.sql` |  |  |
| `cd_custody_pods` | `crates/kiln/src/store/migrations/20261101-0200_w27_cd_custody.sql` |  |  |
| `cd_custody_syncs` | `crates/kiln/src/store/migrations/20261101-0200_w27_cd_custody.sql` |  |  |
| `change_aliases` | `crates/kiln/src/store/migrations/20261026-1000_w26_change_identity.sql` |  |  |
| `change_numbers` | `crates/kiln/src/store/migrations/20261026-1000_w26_change_identity.sql` |  |  |
| `changes` | `crates/kiln/src/store/migrations/20261026-1000_w26_change_identity.sql` |  |  |
| `chat_link_codes` | `crates/kiln/src/store/migrations/20261022-2000_chat_releases.sql` |  |  |
| `chat_links` | `crates/kiln/src/store/migrations/20261022-2000_chat_releases.sql` |  |  |
| `ci_action_policies` | `crates/kiln/src/store/migrations/20261018-0200_w07_action_policies.sql` |  |  |
| `ci_action_tarballs` | `crates/kiln/src/store/migrations/20261019-1900_w07_action_tarballs.sql` |  |  |
| `ci_annotations` | `crates/kiln/src/store/migrations/20261017-2100_w04_runs_v2.sql` |  |  |
| `ci_artifacts` | `crates/kiln/src/store/migrations/20261017-2330_w08_store.sql` |  |  |
| `ci_attempts` | `crates/kiln/src/store/migrations/20261017-2100_w04_runs_v2.sql` |  |  |
| `ci_attestation_subjects` | `crates/kiln/src/store/migrations/20261018-2330_w18_attestations.sql` |  |  |
| `ci_attestation_uploads` | `crates/kiln/src/store/migrations/20261018-2330_w18_attestations.sql` |  |  |
| `ci_attestations` | `crates/kiln/src/store/migrations/20261018-2330_w18_attestations.sql` |  |  |
| `ci_cache_chunks` | `crates/kiln/src/store/migrations/20261019-1700_w08_cache_chunks.sql` |  |  |
| `ci_cache_events` | `crates/kiln/src/store/migrations/20261018-1900_w19_insights.sql` |  |  |
| `ci_caches` | `crates/kiln/src/store/migrations/20261017-2330_w08_store.sql` |  |  |
| `ci_check_runs` | `crates/kiln/src/store/migrations/20261018-0100_w14_records.sql` |  |  |
| `ci_code_scanning_alerts` | `crates/kiln/src/store/migrations/20261019-0130_w14_code_scanning.sql` |  |  |
| `ci_code_scanning_analyses` | `crates/kiln/src/store/migrations/20261019-0130_w14_code_scanning.sql` |  |  |
| `ci_deliveries` | `crates/kiln/src/store/migrations/20261018-0020_w03_deliveries.sql` |  |  |
| `ci_dependency_snapshots` | `crates/kiln/src/store/migrations/20261018-1000_w14_dependency_snapshots.sql` |  |  |
| `ci_deployment_statuses` | `crates/kiln/src/store/migrations/20261018-1100_w15_environments.sql` |  |  |
| `ci_deployments` | `crates/kiln/src/store/migrations/20261018-1100_w15_environments.sql` |  |  |
| `ci_env_reviews` | `crates/kiln/src/store/migrations/20261018-1100_w15_environments.sql` |  |  |
| `ci_environments` | `crates/kiln/src/store/migrations/20261018-1100_w15_environments.sql` |  |  |
| `ci_epoch` | `crates/kiln/src/store/migrations/20261107-0300_w05_epochs.sql` |  |  |
| `ci_events` | `crates/kiln/src/store/migrations/20261017-2100_w04_runs_v2.sql` |  |  |
| `ci_hooks` | `crates/kiln/src/store/migrations/20261019-1500_w03_inbound_hooks.sql` |  |  |
| `ci_images` | `crates/kiln/src/store/migrations/20261019-0110_w13_images.sql` |  |  |
| `ci_inbox` | `crates/kiln/src/store/migrations/20261019-2100_w19_inbox.sql` |  |  |
| `ci_integrations` | `crates/kiln/src/store/migrations/20261018-0500_w11_integrations.sql` |  |  |
| `ci_keys` | `crates/kiln/src/store/migrations/20261018-0030_w09_secrets.sql` |  |  |
| `ci_leg_assignments` | `crates/kiln/src/store/migrations/20261030-0400_w27_leg_results.sql` |  |  |
| `ci_leg_delegations` | `crates/kiln/src/store/migrations/20261030-0400_w27_leg_results.sql` |  |  |
| `ci_leg_pushed` | `crates/kiln/src/store/migrations/20261031-2200_w27_leg_pushed.sql` |  |  |
| `ci_leg_records` | `crates/kiln/src/store/migrations/20261030-0400_w27_leg_results.sql` |  |  |
| `ci_leg_served` | `crates/kiln/src/store/migrations/20261030-0400_w27_leg_results.sql` |  |  |
| `ci_lock_acceptances` | `crates/kiln/src/store/migrations/20261102-1900_w29_lock_approvals.sql` |  |  |
| `ci_lock_approvals` | `crates/kiln/src/store/migrations/20261102-1900_w29_lock_approvals.sql` |  |  |
| `ci_lock_approved_pins` | `crates/kiln/src/store/migrations/20261102-1900_w29_lock_approvals.sql` |  |  |
| `ci_lock_candidate_pins` | `crates/kiln/src/store/migrations/20261102-1900_w29_lock_approvals.sql` |  |  |
| `ci_lock_moves` | `crates/kiln/src/store/migrations/20261102-1700_w29_lock_history.sql` |  |  |
| `ci_lock_refs` | `crates/kiln/src/store/migrations/20261102-1700_w29_lock_history.sql` |  |  |
| `ci_lock_review_pins` | `crates/kiln/src/store/migrations/20261102-1800_w29_lock_reviews.sql` |  |  |
| `ci_lock_reviews` | `crates/kiln/src/store/migrations/20261102-1800_w29_lock_reviews.sql` |  |  |
| `ci_lock_sources` | `crates/kiln/src/store/migrations/20261102-1700_w29_lock_history.sql` |  |  |
| `ci_log_cursor` | `crates/kiln/src/store/migrations/20261106-2104_dbq_ci.sql` | `crates/kiln/src/store/migrations/20261106-2100_dbq_converge.sql` |  |
| `ci_log_index` | `crates/kiln/src/ci/logsearch.rs`, `crates/kiln/src/store/migrations/20261018-2100_w19_log_search.sql` |  |  |
| `ci_log_indexed` | `crates/kiln/src/ci/logsearch.rs`, `crates/kiln/src/store/migrations/20261018-2100_w19_log_search.sql` |  |  |
| `ci_log_pending` | `crates/kiln/src/store/migrations/20261106-2104_dbq_ci.sql` | `crates/kiln/src/store/migrations/20261106-2100_dbq_converge.sql` |  |
| `ci_oidc_subjects` | `crates/kiln/src/store/migrations/20261018-1400_w10_oidc_subjects.sql` |  |  |
| `ci_pending_updates` | `crates/kiln/src/store/migrations/20261101-1200_w29_updates.sql` |  |  |
| `ci_pipeline_drafts` | `crates/kiln/src/store/migrations/20261101-0800_w29_pipeline_drafts.sql` |  |  |
| `ci_plan_log` | `crates/kiln/src/store/migrations/20261017-2100_w04_runs_v2.sql` |  |  |
| `ci_plans` | `crates/kiln/src/store/migrations/20261017-2100_w04_runs_v2.sql` |  |  |
| `ci_pool_samples` | `crates/kiln/src/store/migrations/20261020-0200_w21_pool_samples.sql` |  |  |
| `ci_pools` | `crates/kiln/src/store/migrations/20261019-0120_w13_pools.sql` |  |  |
| `ci_preview_removals` | `crates/kiln/src/store/migrations/20261021-0100_w15_previews.sql` |  |  |
| `ci_previews` | `crates/kiln/src/store/migrations/20261021-0100_w15_previews.sql` |  |  |
| `ci_pushes` | `crates/kiln/src/store/migrations/20261019-2200_w10_started_by.sql` |  |  |
| `ci_release_assets` | `crates/kiln/src/store/migrations/20261018-0100_w14_records.sql` |  |  |
| `ci_released_tags` | `crates/kiln/src/store/migrations/20261101-1100_w29_releases.sql` |  |  |
| `ci_releases` | `crates/kiln/src/store/migrations/20261018-0100_w14_records.sql` |  |  |
| `ci_reuse` | `crates/kiln/src/store/migrations/20261018-2350_w08_reuse.sql` |  |  |
| `ci_rollup_done` | `crates/kiln/src/store/migrations/20261018-1900_w19_insights.sql` |  |  |
| `ci_rollups` | `crates/kiln/src/store/migrations/20261018-1900_w19_insights.sql` |  |  |
| `ci_routing_rules` | `crates/kiln/src/store/migrations/20261018-0900_w05_routing_rules.sql` |  |  |
| `ci_run_attempts` | `crates/kiln/src/store/migrations/20261020-0500_w20_run_attempts.sql` |  |  |
| `ci_runner_time` | `crates/kiln/src/store/migrations/20261018-1900_w19_insights.sql` |  |  |
| `ci_schedule_fires` | `crates/kiln/src/store/migrations/20261018-0010_w03_schedules.sql` |  |  |
| `ci_secret_outputs` | `crates/kiln/src/store/migrations/20261018-0400_w04_secret_outputs.sql` |  |  |
| `ci_secret_uses` | `crates/kiln/src/store/migrations/20261018-0030_w09_secrets.sql` |  |  |
| `ci_secrets` | `crates/kiln/src/store/migrations/20261018-0030_w09_secrets.sql` |  |  |
| `ci_statuses` | `crates/kiln/src/store/migrations/20261018-0100_w14_records.sql` |  |  |
| `ci_summaries` | `crates/kiln/src/store/migrations/20261017-2100_w04_runs_v2.sql` |  |  |
| `ci_uses` | `crates/kiln/src/store/migrations/20261101-1000_w29_uses.sql` |  |  |
| `ci_variables` | `crates/kiln/src/store/migrations/20261018-0030_w09_secrets.sql` |  |  |
| `ci_workflows` | `crates/kiln/src/store/migrations/20261017-1200_w02_workflows.sql` |  |  |
| `cluster_conf` | `crates/kiln/src/store/group.rs` |  |  |
| `cluster_mark` | `crates/kiln/src/store/group.rs` | `crates/kiln/src/store/group.rs` |  |
| `cluster_state` | `crates/kiln/src/store/group.rs` | `crates/kiln/src/store/group.rs` |  |
| `confirms` | `crates/kiln/src/store/migrations/20261114-2200_confirms.sql` |  |  |
| `copied` | `crates/kiln/src/convo/sql.rs` |  |  |
| `custody` | `crates/kiln/src/store/migrations/20261030-0100_w27_custody.sql` |  |  |
| `custody_breaks` | `crates/kiln/src/store/migrations/20261030-0100_w27_custody.sql` |  |  |
| `custody_follows` | `crates/kiln/src/store/migrations/20261030-0100_w27_custody.sql` |  |  |
| `custody_open_passes` | `crates/kiln/src/store/migrations/20261106-2106_dbq_relays.sql` |  |  |
| `custody_relay_streams` | `crates/kiln/src/store/migrations/20261106-2106_dbq_relays.sql` |  |  |
| `custody_taken_for` | `crates/kiln/src/store/migrations/20261106-2102_dbq_custody.sql` |  |  |
| `custody_takers` | `crates/kiln/src/store/migrations/20261106-2102_dbq_custody.sql` |  |  |
| `custody_things` | `crates/kiln/src/store/migrations/20261030-0100_w27_custody.sql` |  |  |
| `d` | `crates/kiln/src/store/migrations.rs` |  | yes |
| `dbq_custody_kept` | `crates/kiln/src/store/migrations/20261106-2102_dbq_custody.sql` | `crates/kiln/src/store/migrations/20261106-2102_dbq_custody.sql` |  |
| `dbq_principals` | `crates/kiln/src/ci/review.rs`, `crates/kiln/src/store/migrations/20261106-2104_dbq_ci.sql` | `crates/kiln/src/ci/review.rs`, `crates/kiln/src/store/migrations/20261106-2104_dbq_ci.sql` |  |
| `dbq_relays_passes` | `crates/kiln/src/store/migrations/20261106-2106_dbq_relays.sql` | `crates/kiln/src/store/migrations/20261106-2106_dbq_relays.sql` |  |
| `deploy_apps` | `crates/kiln/src/store/migrations/20261019-0100_cd_deploy_api.sql` |  |  |
| `deploy_environments` | `crates/kiln/src/store/migrations/20261019-0100_cd_deploy_api.sql` |  |  |
| `deploy_fleet_proposals` | `crates/kiln/src/store/migrations/20261022-1900_p5_fleet_proposals.sql` |  |  |
| `deploy_registrations` | `crates/kiln/src/store/migrations/20261019-0100_cd_deploy_api.sql` |  |  |
| `deploy_releases` | `crates/kiln/src/store/migrations/20261019-0100_cd_deploy_api.sql` |  |  |
| `deploy_templates` | `crates/kiln/src/store/migrations/20261019-0130_cd_deploy_templates.sql` |  |  |
| `deploy_work` | `crates/kiln/src/store/migrations/20261019-0100_cd_deploy_api.sql` |  |  |
| `diff_labels` | `crates/kiln/src/store/migrations/20261019-1400_w14_pull_labels.sql` |  |  |
| `diff_reviewers` | `crates/kiln/src/store/migrations/20261019-0010_w14_diffs.sql` |  |  |
| `diff_version_commits` | `crates/kiln/src/store/migrations/20261026-0100_w26_ship_review.sql` |  |  |
| `diff_versions` | `crates/kiln/src/store/migrations/20261019-0010_w14_diffs.sql` |  |  |
| `diffs` | `crates/kiln/src/store/migrations/20261019-0010_w14_diffs.sql` |  |  |
| `drops` | `crates/kiln/src/convo/sql.rs` |  |  |
| `enrollments` | `crates/kiln/src/store/migrations/20261026-1800_w27_identities.sql` |  |  |
| `event_outbox` | `crates/kiln/src/store/migrations/20261107-1500_cp_t2_2_event_outbox.sql` |  |  |
| `event_positions` | `crates/kiln/src/store/migrations/20261107-1501_cp_t2_2_event_deliveries.sql` |  |  |
| `event_relay` | `crates/kiln/src/store/migrations/20261107-1500_cp_t2_2_event_outbox.sql` |  |  |
| `events` | `crates/kiln/src/convo/sql.rs` |  |  |
| `github_app` | `crates/kiln/src/store/migrations/20261019-0050_w14_github_app.sql` |  |  |
| `github_checks` | `crates/kiln/src/store/migrations/20261019-0050_w14_github_app.sql` |  |  |
| `github_manifests` | `crates/kiln/src/store/migrations/20261019-0050_w14_github_app.sql` |  |  |
| `goal_items` | `crates/kiln/src/store/migrations/20261017-2100_agents_goal_stacks.sql` |  |  |
| `goal_stacks` | `crates/kiln/src/store/migrations/20261017-2100_agents_goal_stacks.sql` |  |  |
| `grants` | `crates/kiln/src/store/migrations/20261018-0800_w10_principals.sql` |  |  |
| `grown` | `crates/kiln/src/store.rs` |  | yes |
| `host_pull_commits` | `crates/kiln/src/store/migrations/20261026-0200_w26_host_pulls.sql` |  |  |
| `host_pull_heads` | `crates/kiln/src/store/migrations/20261026-0200_w26_host_pulls.sql` |  |  |
| `host_pulls` | `crates/kiln/src/store/migrations/20261026-0200_w26_host_pulls.sql` |  |  |
| `host_verifications` | `crates/kiln/src/store/migrations/20261030-2330_w27_host_verifications.sql` |  |  |
| `identities` | `crates/kiln/src/store/migrations/20261026-1800_w27_identities.sql` |  |  |
| `inbox` | `crates/kiln/src/convo/sql.rs` |  |  |
| `jobs` | `crates/kiln/src/ci/logsearch.rs`, `crates/kiln/src/store.rs`, `crates/kiln/src/store/capture.rs` |  | yes |
| `kiln_meta` | `crates/kiln/src/store/migrations.rs` |  |  |
| `kv` | `crates/kiln/src/harness/told.rs`, `crates/kiln/src/store.rs` |  | yes |
| `land_queue` | `crates/kiln/src/store/migrations/20261019-0020_w14_review.sql` |  |  |
| `land_records` | `crates/kiln/src/store/migrations/20261026-1700_w28_land_wire.sql` |  |  |
| `land_signatures` | `crates/kiln/src/store/migrations/20261031-0200_w27_land_signatures.sql` |  |  |
| `land_when_ready` | `crates/kiln/src/store/migrations/20261021-0300_w14_land_when_ready.sql` |  |  |
| `local` | `crates/kiln/src/store/capture.rs` |  | yes |
| `machine_names` | `crates/kiln/src/store/migrations/20261104-1200_machines_names.sql` |  |  |
| `message_nodes` | `crates/kiln/src/aisessions.rs`, `crates/kiln/src/aisessions/devin.rs` |  | yes |
| `messages` | `crates/kiln/src/store/storage_tests.rs` |  | yes |
| `meta` | `crates/kiln/src/convo/sql.rs`, `crates/kiln/src/records/turso.rs` |  |  |
| `metrics` | `crates/kiln/src/store/migrations/20261018-1600_w16_metrics.sql` |  |  |
| `migrations` | `crates/kiln/src/store/migrations.rs`, `crates/kiln/src/store/migrations/20261017-0000_ci_baseline.sql` |  |  |
| `notice_marks` | `crates/kiln/src/store/migrations/20261023-1600_notice_marks.sql` |  |  |
| `notify_deliveries` | `crates/kiln/src/store/migrations/20261018-1700_w19_notify.sql` |  |  |
| `notify_sent` | `crates/kiln/src/store/migrations/20261018-1700_w19_notify.sql` |  |  |
| `notify_subscriptions` | `crates/kiln/src/store/migrations/20261019-2210_w19_subscriptions.sql` |  |  |
| `oidc_keys` | `crates/kiln/src/store/migrations/20261018-0040_w10_oidc_keys.sql` |  |  |
| `ok` | `crates/kiln/src/store/migrations.rs` |  | yes |
| `other` | `crates/kiln/src/aisessions/devin.rs` |  | yes |
| `pairs` | `crates/kiln/src/store/capture.rs` |  | yes |
| `passkeys` | `crates/kiln/src/store/migrations/20261019-2300_w10_passkeys.sql` |  |  |
| `peers` | `crates/kiln/src/convo/sql.rs` |  |  |
| `plan_reviews` | `crates/kiln/src/store/migrations/20261031-1200_plan_reviews.sql` |  |  |
| `plan_run_steps` | `crates/kiln/src/store/migrations/20261019-0900_agentic_plan_agents.sql` |  |  |
| `plan_runs` | `crates/kiln/src/store/migrations/20261017-1900_agents_plans.sql` |  |  |
| `plan_tags` | `crates/kiln/src/store/migrations/20261106-2107_dbq_agents_structure.sql` |  |  |
| `plan_versions` | `crates/kiln/src/store/migrations/20261017-1900_agents_plans.sql` |  |  |
| `plans` | `crates/kiln/src/store/migrations/20261017-1900_agents_plans.sql`, `crates/kiln/src/store/migrations/20261106-2107_dbq_agents_structure.sql` | `crates/kiln/src/store/migrations/20261106-2107_dbq_agents_structure.sql` |  |
| `plans_by_list` | `crates/kiln/src/store/migrations/20261106-2107_dbq_agents_structure.sql` | `crates/kiln/src/store/migrations/20261106-2107_dbq_agents_structure.sql` |  |
| `prebuilds` | `crates/kiln/src/store/migrations/20261116-1900_p4_prebuilds.sql` |  |  |
| `proxy_cache` | `crates/kiln/src/store/migrations/20261019-0100_w12_proxy.sql` |  |  |
| `push_mutes` | `crates/kiln/src/store/migrations/20261019-0100_n1_push.sql` |  |  |
| `push_said` | `crates/kiln/src/store/migrations/20261019-0100_n1_push.sql` |  |  |
| `push_subscriptions` | `crates/kiln/src/store/migrations/20261019-0100_n1_push.sql` |  |  |
| `reapi_blobs` | `crates/kiln/src/store/migrations/20261027-1900_w24_reapi.sql` |  |  |
| `records` | `crates/kiln/src/convo/sql.rs` | `crates/kiln/src/convo/sql.rs` |  |
| `records_families` | `crates/kiln/src/store/migrations/20261023-1300_p6_records.sql` |  |  |
| `records_meta` | `crates/kiln/src/store/migrations/20261023-1300_p6_records.sql` |  |  |
| `records_versions` | `crates/kiln/src/store/migrations/20261023-1300_p6_records.sql` |  |  |
| `refs` | `crates/kiln/src/store.rs` |  | yes |
| `registry_blobs` | `crates/kiln/src/store/migrations/20261019-0090_w12_registry.sql` |  |  |
| `registry_manifests` | `crates/kiln/src/store/migrations/20261019-0090_w12_registry.sql` |  |  |
| `registry_policies` | `crates/kiln/src/store/migrations/20261022-1800_p5_registry_policies.sql` |  |  |
| `registry_repos` | `crates/kiln/src/store/migrations/20261019-0090_w12_registry.sql` |  |  |
| `registry_tags` | `crates/kiln/src/store/migrations/20261019-0090_w12_registry.sql` |  |  |
| `registry_uploads` | `crates/kiln/src/store/migrations/20261019-0090_w12_registry.sql` |  |  |
| `release_chat_posts` | `crates/kiln/src/store/migrations/20261022-2000_chat_releases.sql` |  |  |
| `repo_ci` | `crates/kiln/src/store/migrations/20261019-0060_w23_providers.sql` |  |  |
| `repos` | `crates/kiln/src/store.rs` |  | yes |
| `research` | `crates/kiln/src/store/migrations/20261022-1000_research_goal_plans.sql` |  |  |
| `review_agent_fixes` | `crates/kiln/src/store/migrations/20261031-2000_w26_agent_fixes.sql` |  |  |
| `review_chat_posts` | `crates/kiln/src/store/migrations/20261021-0200_w14_replies.sql` |  |  |
| `review_collisions` | `crates/kiln/src/store/migrations/20261030-0100_w26_collisions.sql` |  |  |
| `review_comments` | `crates/kiln/src/store/migrations/20261019-0020_w14_review.sql` |  |  |
| `review_conflicts` | `crates/kiln/src/store/migrations/20261026-2200_w26_conflicts.sql` |  |  |
| `review_held` | `crates/kiln/src/store/migrations/20261026-1200_w26_held_review.sql` |  |  |
| `review_ladder_switches` | `crates/kiln/src/store/migrations/20261029-2300_w26_ladder.sql` |  |  |
| `review_ladders` | `crates/kiln/src/store/migrations/20261029-2300_w26_ladder.sql` |  |  |
| `review_pull_replays` | `crates/kiln/src/store/migrations/20261029-2200_w26_pull_replays.sql` |  |  |
| `review_remembered` | `crates/kiln/src/store/migrations/20261029-2300_w26_ladder.sql` |  |  |
| `review_replays` | `crates/kiln/src/store/migrations/20261026-2200_w26_conflicts.sql` |  |  |
| `review_reply_tokens` | `crates/kiln/src/store/migrations/20261021-0200_w14_replies.sql` |  |  |
| `review_resolutions` | `crates/kiln/src/store/migrations/20261029-2300_w26_ladder.sql` |  |  |
| `review_signatures` | `crates/kiln/src/store/migrations/20261031-0100_w27_review_signatures.sql` |  |  |
| `review_threads` | `crates/kiln/src/store/migrations/20261019-0020_w14_review.sql` |  |  |
| `reviews` | `crates/kiln/src/store/migrations/20261019-0020_w14_review.sql` |  |  |
| `revocations` | `crates/kiln/src/store/migrations/20261026-1800_w27_identities.sql` |  |  |
| `roles` | `crates/kiln/src/store/migrations/20261018-0800_w10_principals.sql` |  |  |
| `run_model` | `crates/kiln/src/store/migrations/20261107-0400_w05_run_observations.sql` |  |  |
| `run_observations` | `crates/kiln/src/store/migrations/20261107-0400_w05_run_observations.sql` |  |  |
| `run_pruned` | `crates/kiln/src/store/migrations/20261107-0400_w05_run_observations.sql` |  |  |
| `run_size_rollups` | `crates/kiln/src/store/migrations/20261107-0500_e6_run_sizes.sql` |  |  |
| `run_sizes` | `crates/kiln/src/store/migrations/20261107-0500_e6_run_sizes.sql` |  |  |
| `run_speed_cells` | `crates/kiln/src/store/migrations/20261108-0100_w05_run_speed_cells.sql` |  |  |
| `runs` | `crates/kiln/src/ci/logsearch.rs`, `crates/kiln/src/store.rs`, `crates/kiln/src/store/capture.rs` |  | yes |
| `seam_leases` | `crates/kiln/src/store/migrations/20261113-0600_cp_t8_seam_secrets.sql` |  |  |
| `seam_secret_versions` | `crates/kiln/src/store/migrations/20261113-0600_cp_t8_seam_secrets.sql` |  |  |
| `seam_secrets` | `crates/kiln/src/store/migrations/20261113-0600_cp_t8_seam_secrets.sql` |  |  |
| `secret_keys` | `crates/kiln/src/store/migrations/20261019-1600_secrets_store.sql` |  |  |
| `secret_reads` | `crates/kiln/src/store/migrations/20261019-1600_secrets_store.sql` |  |  |
| `secret_versions` | `crates/kiln/src/store/migrations/20261019-1600_secrets_store.sql` |  |  |
| `sessions` | `crates/kiln/src/aisessions.rs`, `crates/kiln/src/aisessions/devin.rs`, `crates/kiln/src/convo/sql.rs` |  |  |
| `ship_changes` | `crates/kiln/src/ship.rs`, `crates/kiln/src/store/migrations/20261025-0100_ship_changes.sql` | `crates/kiln/src/ship.rs` |  |
| `ship_changes_away` | `crates/kiln/src/ship.rs` | `crates/kiln/src/ship.rs` | yes |
| `ship_handoffs` | `crates/kiln/src/store/migrations/20261029-0100_w26_handoffs.sql` |  |  |
| `ship_order` | `crates/kiln/src/store/migrations/20261025-0100_ship_changes.sql` |  |  |
| `ship_seq` | `crates/kiln/src/store/migrations/20261025-0100_ship_changes.sql` |  |  |
| `skill_drafts` | `crates/kiln/src/store/migrations/20261109-1200_skills_install.sql` |  |  |
| `skill_packages` | `crates/kiln/src/store/migrations/20261109-1200_skills_install.sql` |  |  |
| `slates_fleet_bootstrap` | `crates/kiln/src/store/migrations/20261120-0900_slates_fleet_bootstrap.sql` |  |  |
| `ssh_signers` | `crates/kiln/src/store/migrations/20261030-1700_w27_ssh_signers.sql` |  |  |
| `sso_states` | `crates/kiln/src/store/migrations/20261019-0080_w23_sso.sql` |  |  |
| `stacks` | `crates/kiln/src/store/migrations/20261019-0010_w14_diffs.sql` |  |  |
| `steps` | `crates/kiln/src/ci/logsearch.rs`, `crates/kiln/src/store.rs` |  | yes |
| `subjects` | `crates/kiln/src/store/capture.rs` |  | yes |
| `t` | `crates/kiln/src/memory/owners.rs`, `crates/kiln/src/os/alloc.rs`, `crates/kiln/src/store/indexes.rs` |  | yes |
| `test_cases` | `crates/kiln/src/store/migrations/20261018-1500_w16_test_results.sql` |  |  |
| `test_reports` | `crates/kiln/src/store/migrations/20261018-1500_w16_test_results.sql` |  |  |
| `test_results` | `crates/kiln/src/store/migrations/20261018-1500_w16_test_results.sql` |  |  |
| `thread_goals` | `crates/kiln/src/harness/homes.rs`, `crates/kiln/src/harness/moving.rs` |  | yes |
| `tokens` | `crates/kiln/src/store/migrations/20261018-0050_w10_tokens.sql` |  |  |
| `told_runs` | `crates/kiln/src/store/migrations/20261107-0600_told_runs.sql` |  |  |
| `trail` | `crates/kiln/src/store/migrations/20261031-0300_w27_trail.sql` |  |  |
| `trail_anchors` | `crates/kiln/src/store/migrations/20261031-0300_w27_trail.sql` |  |  |
| `trail_checkpoints` | `crates/kiln/src/store/migrations/20261031-0300_w27_trail.sql` |  |  |
| `trail_nodes` | `crates/kiln/src/store/migrations/20261031-0300_w27_trail.sql` |  |  |
| `trail_signatures` | `crates/kiln/src/store/migrations/20261031-0300_w27_trail.sql` |  |  |
| `trail_subjects` | `crates/kiln/src/store/migrations/20261031-0300_w27_trail.sql` |  |  |
| `transcript_archives` | `crates/kiln/src/store/migrations/20261021-1200_t10_blobs.sql` |  |  |
| `transcript_segments` | `crates/kiln/src/store/migrations/20261021-1200_t10_blobs.sql` |  |  |
| `transfer_refusals` | `crates/kiln/src/store/migrations/20261030-0100_w27_custody.sql` |  |  |
| `transfers_unsigned` | `crates/kiln/src/store/migrations/20261030-0100_w27_custody.sql` |  |  |
| `u` | `crates/kiln/src/store/indexes.rs` |  | yes |
| `user_group_members` | `crates/kiln/src/authz/groups.rs`, `crates/kiln/src/store/migrations/20261022-1700_p5_groups.sql` |  |  |
| `user_groups` | `crates/kiln/src/authz/groups.rs`, `crates/kiln/src/store/migrations/20261022-1700_p5_groups.sql` |  |  |
| `user_identities` | `crates/kiln/src/authz/groups.rs`, `crates/kiln/src/store/migrations/20261019-0080_w23_sso.sql` |  |  |
| `users` | `crates/kiln/src/store/migrations/20261018-0800_w10_principals.sql` |  |  |
| `versions` | `crates/kiln/src/records/turso.rs` |  |  |
| `viewed_files` | `crates/kiln/src/store/migrations/20261019-0020_w14_review.sql` |  |  |
| `webhook_deliveries` | `crates/kiln/src/store/migrations/20261018-1800_w19_webhooks.sql` |  |  |
| `work_flight` | `crates/kiln/src/store/migrations/20261026-0100_w28_flight.sql` |  |  |
| `work_reap_policies` | `crates/kiln/src/store/migrations/20261026-0300_w24_reap_policies.sql` |  |  |
| `work_records` | `crates/kiln/src/store/migrations/20261025-0215_w24_work.sql` |  |  |
| `x` | `crates/kiln/src/store/migrations.rs` |  | yes |

## JSON files kiln writes

| File | Written in | Tests only |
|---|---|---|
| `../../ui/tests/bp-measure.json` | `crates/kiln/src/bp/measure.rs` |  |
| `../../ui/tests/fixtures/attention-lean.json` | `crates/kiln/src/harness/attention/lean.rs` | yes |
| `../../ui/tests/fixtures/columns.json` | `crates/kiln/src/wire/columns.rs` | yes |
| `.claude-plugin/plugin.json` | `crates/kiln/src/skills/place.rs` | yes |
| `.credentials.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `.gemini/antigravity-cli/settings.json` | `crates/kiln/src/harness/kinds.rs` | yes |
| `.gemini/config/hooks.json` | `crates/kiln/src/harness/kinds.rs` | yes |
| `.gemini/config/mcp_config.json` | `crates/kiln/src/harness/kinds.rs`, `crates/kiln/src/sandbox/isolated.rs` | yes |
| `.keep.json` | `crates/kiln/src/reach/keeper.rs` |  |
| `.sigstore.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `/../../docs/work-ops.schema.json` | `crates/kiln-ci/src/work/ops/schema.rs` | yes |
| `/home/claude/.claude/settings.json` | `crates/kiln/src/sandbox/isolated.rs` | yes |
| `/home/h/x.json` | `crates/kiln/src/harness/session.rs` | yes |
| `/home/me/.gemini/config/mcp_config.json` | `crates/kiln/src/sandbox/isolated.rs` | yes |
| `/home/runner/work/_temp/_github_workflow/event.json` | `crates/kiln/src/runner/runtime.rs` | yes |
| `/state/slates-fleet/fleet.json` | `crates/kiln/src/workspace/slates.rs` | yes |
| `/vm.json` | `crates/kiln/src/runner/firecracker.rs` |  |
| `0.json` | `crates/kiln/src/runner/warm.rs` | yes |
| `1.json` | `crates/kiln/src/harness/moving.rs`, `crates/kiln/src/observe/incidents/mod.rs` | yes |
| `2.json` | `crates/kiln/src/observe/incidents/mod.rs` | yes |
| `3.json` | `crates/kiln/src/observe/incidents/mod.rs` | yes |
| `4.json` | `crates/kiln/src/observe/incidents/mod.rs` | yes |
| `_kiln_build_{idx}.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `a.json` | `crates/kiln/src/ci/pushsig.rs`, `crates/kiln/src/harness/host.rs` | yes |
| `agent-handover.json` | `crates/kiln/src/harness/host.rs`, `crates/kiln/src/harness/inbox/faults.rs` |  |
| `agent-limit.json` | `crates/kiln/src/harness/limit.rs` |  |
| `antigravity-cli/settings.json` | `crates/kiln/src/harness/kinds.rs` | yes |
| `archives.json` | `crates/kiln/src/observe/rehydrate.rs` | yes |
| `attachments/uploads/{id}.json` | `crates/kiln/src/harness/registry.rs` | yes |
| `auth.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `b.json` | `crates/kiln/src/harness/host.rs` | yes |
| `bar.json` | `crates/kiln-lab/src/bar.rs` |  |
| `binaries/kiln-{target}.json` | `crates/kiln/src/updates.rs` | yes |
| `broken.json` | `crates/kiln/src/harness/isolated/home.rs` | yes |
| `builds.json` | `crates/kiln/src/fleet.rs` |  |
| `c.json` | `crates/kiln/src/harness/host.rs` | yes |
| `chat.json` | `crates/kiln/src/aisessions.rs` | yes |
| `clouds.json` | `crates/kiln/src/observe/cloud/mod.rs` |  |
| `config.json` | `crates/kiln/src/runner/auth.rs`, `crates/kiln/src/runner/runtime.rs` |  |
| `config.v2.json` | `crates/kiln/src/observe/collect/files.rs` |  |
| `config/hooks.json` | `crates/kiln/src/harness/kinds.rs` | yes |
| `context.json` | `crates/kiln/src/ci/pushsig.rs` |  |
| `copies.json` | `crates/kiln/src/sync/runner.rs` |  |
| `d.json` | `crates/kiln/src/observe/collect/files.rs` | yes |
| `deploy/api/task.json` | `crates/kiln-cd/src/apps.rs` | yes |
| `epochs.json` | `crates/kiln/src/harness/registry.rs` | yes |
| `estimates.json` | `crates/kiln/src/ci/cli.rs` |  |
| `event.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `eventhubs-{name}.json` | `crates/kiln/src/observe/cloud/mod.rs` |  |
| `extension/manifest.json` | `crates/kiln/src/browser/stream.rs` |  |
| `files/s1.json` | `crates/kiln/src/harness/kept.rs` | yes |
| `fixtures/shards-groups.json` | `crates/kiln/src/reach/place.rs` | yes |
| `gone.json` | `crates/kiln/src/headroom/pace.rs` | yes |
| `guard.json` | `crates/kiln/src/protect.rs` | yes |
| `handovers-decided.json` | `crates/kiln/src/harness/host.rs` |  |
| `handovers-taken.json` | `crates/kiln/src/harness/host.rs`, `crates/kiln/src/harness/inbox/faults.rs` |  |
| `harness-models.json` | `crates/kiln/src/harness/host.rs` | yes |
| `harness-sessions.json` | `crates/kiln/src/harness/host.rs` | yes |
| `harness.json` | `crates/kiln/src/agent.rs`, `crates/kiln/src/harness/host.rs`, `crates/kiln/src/harness/host/transfer/native.rs`, `crates/kiln/src/harness/inbox/faults.rs`, `crates/kiln/src/harness/isolated/home.rs`, `crates/kiln/src/harness/kinds.rs` |  |
| `harness/antigravity.d/osi.json` | `crates/kiln/src/sandbox/isolated.rs` | yes |
| `harness/codex.d/osi.json` | `crates/kiln/src/sandbox/isolated.rs` | yes |
| `hold.json` | `crates/kiln-lab/src/bar.rs` |  |
| `holders.json` | `crates/kiln/src/harness/registry.rs` | yes |
| `hooks/hooks.json` | `crates/kiln/src/skills/place.rs` | yes |
| `identity.json` | `crates/kiln/src/ci/dependency_sources.rs` | yes |
| `incidents/9.json` | `crates/kiln/src/observe/incidents/mod.rs` | yes |
| `incidents/{}.json` | `crates/kiln/src/observe/incidents/mod.rs` | yes |
| `indexes.json` | `crates/kiln/src/observe/indexes/mod.rs` |  |
| `instance.json` | `crates/kiln/src/agent.rs`, `crates/kiln/src/upgrade.rs` |  |
| `integrations.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `jail.json` | `crates/kiln/src/runner/firecracker.rs`, `crates/kiln/src/runner/jail.rs` |  |
| `job.json` | `crates/kiln-lab/src/browser/run.rs` |  |
| `k.json` | `crates/kiln/src/observe/collect/files.rs` | yes |
| `keep-off.json` | `crates/kiln/src/harness/registry.rs` | yes |
| `kiln-mcp.json` | `crates/kiln/src/mcp/mod.rs` |  |
| `kiln-{t}.json` | `crates/kiln/src/updates.rs` | yes |
| `lane.json` | `crates/kiln/src/runner/warm.rs` |  |
| `live.json` | `crates/kiln/src/headroom/pace.rs` | yes |
| `local.json` | `crates/kiln/src/harness/registry.rs` | yes |
| `manifest.json` | `crates/kiln/src/browser/stream.rs`, `crates/kiln/src/runner/rootfs.rs` |  |
| `member.json` | `crates/kiln/src/observe/owner.rs` |  |
| `members.json` | `crates/kiln/src/observe/owner.rs` |  |
| `metadata.from-lab-a.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `metadata.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `mirrors.json` | `crates/kiln/src/convo/mirror.rs`, `crates/kiln/src/harness/registry.rs` | yes |
| `moved-git.json` | `crates/kiln/src/harness/moving.rs` |  |
| `names.json` | `crates/kiln/src/harness/registry.rs` | yes |
| `next.json` | `crates/kiln/src/observe/incidents/mod.rs` |  |
| `oauth_creds.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `openapi.json` | `crates/kiln/src/api.rs` |  |
| `opencode.json` | `crates/kiln/src/sandbox/isolated.rs` | yes |
| `other-agent-other.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `p.json` | `crates/kiln/src/observe/collect/files.rs` | yes |
| `pace.json` | `crates/kiln/src/sense/pace.rs` |  |
| `paced.json` | `crates/kiln/src/headroom/pace.rs` |  |
| `pacer.json` | `crates/kiln/src/headroom/pace.rs` | yes |
| `package.json` | `crates/kiln/src/harness/host.rs`, `crates/kiln/src/harness/inbox/faults.rs` |  |
| `pins.json` | `crates/kiln/src/reach/e2e.rs` |  |
| `places.json` | `crates/kiln/src/observe/collect/files.rs` | yes |
| `plugin.json` | `crates/kiln/src/mcp/mod.rs` |  |
| `profile-checks.json` | `crates/kiln-lab/src/browser/check.rs` |  |
| `projects.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `provenance.sigstore.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `reads.json` | `crates/kiln/src/runner/rustc.rs` |  |
| `record.json` | `crates/kiln-lab/src/browser/record.rs` |  |
| `rehydrations.json` | `crates/kiln/src/observe/rehydrate.rs` |  |
| `replaces.json` | `crates/kiln/src/updates.rs`, `crates/kiln/src/upgrade.rs` |  |
| `result.json` | `crates/kiln-lab/src/browser/run.rs` |  |
| `run.json` | `crates/kiln-lab/src/browser/run.rs`, `crates/kiln-lab/src/main.rs` |  |
| `runner-engine.json` | `crates/kiln/src/harness/engine.rs` |  |
| `runs.json` | `crates/kiln/src/runner/warm.rs` |  |
| `s1-agent-s1.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `same-chat.json` | `crates/kiln/src/harness/host.rs` | yes |
| `sample.json` | `crates/kiln/src/ci/cli.rs` |  |
| `sbom.cdx.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `sbom.sigstore.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `sbom.spdx.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `scrapes.json` | `crates/kiln/src/observe/scrape.rs` |  |
| `search-meta.json` | `crates/kiln/src/ci/search.rs` |  |
| `secrets.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `ses_1.json` | `crates/kiln/src/harness/moving.rs` | yes |
| `sessions.json` | `crates/kiln/src/harness/session.rs` | yes |
| `shaped-{}.json` | `crates/kiln/src/ci/bench.rs` |  |
| `state-{member}.json` | `crates/kiln/src/sync/group.rs` | yes |
| `supervise-x.json` | `crates/kiln/src/supervise.rs` | yes |
| `supervise-{}.json` | `crates/kiln/src/supervise.rs` |  |
| `target/ci-bench.json` | `crates/kiln/src/ci/bench.rs` |  |
| `task.json` | `crates/kiln-cd/src/apps.rs` | yes |
| `tasks.json` | `crates/kiln/src/sync/tasks.rs` |  |
| `terminal-handover.json` | `crates/kiln/src/agent.rs` | yes |
| `times_ms.json` | `crates/kiln-lab/src/browser/run.rs` |  |
| `told.json` | `crates/kiln/src/reach/told.rs`, `crates/kiln/src/reach/told/tests.rs` |  |
| `u.json` | `crates/kiln/src/observe/collect/files.rs` | yes |
| `ui/node_modules/x/package.json` | `crates/kiln/src/ci/graphs.rs` | yes |
| `ui/package.json` | `crates/kiln/src/updates.rs` | yes |
| `update-available.json` | `crates/kiln/src/agent.rs` | yes |
| `vm.json` | `crates/kiln/src/runner/firecracker.rs` |  |
| `{base}/index.json` | `crates/kiln/src/runner/runtime.rs` |  |
| `{dir}.d/osi.json` | `crates/kiln/src/sandbox/isolated.rs` |  |
| `{hex}.config.json` | `crates/kiln/src/runner/rootfs.rs` |  |
| `{hex}.json` | `crates/kiln/src/runner/rootfs.rs` |  |
| `{id}.json` | `crates/kiln/src/harness/host.rs`, `crates/kiln/src/harness/visuals.rs`, `crates/kiln/src/runner/runtime.rs` |  |
| `{key}.json` | `crates/kiln/src/reach/node.rs` | yes |
| `{name}.json` | `crates/kiln/src/harness/isolated/home.rs` |  |
| `{n}.json` | `crates/kiln/src/runner/warm.rs` |  |
| `{out}/claude-to-codex.json` | `crates/kiln/src/harness/handoff.rs` | yes |
| `{owner}.json` | `crates/kiln/src/skills/place.rs` |  |
| `{project}.json` | `crates/kiln/src/runner/firecracker.rs` |  |
| `{public_id}.json` | `crates/kiln/src/observe/incidents/mod.rs` |  |
| `{safe}-{}.json` | `crates/kiln/src/sync/runner.rs` |  |
| `{safe}.json` | `crates/kiln/src/mcp/mod.rs` |  |
| `{}-gcp.json` | `crates/kiln/src/runner/auth.rs` |  |
| `{}-{}.json` | `crates/kiln/src/headroom/pace.rs` |  |
| `{}.json` | `crates/kiln/src/ci/pushsig.rs`, `crates/kiln/src/harness/kept.rs`, `crates/kiln/src/runner/firecracker.rs`, `crates/kiln/src/skills/place.rs` |  |
| `{}/.gemini/config/mcp_config.json` | `crates/kiln/src/sandbox/isolated.rs` |  |
