from pathlib import Path
import json,subprocess
root=Path("/private/tmp/kiln-queue-prevention-20261111");out=root/"docs/research/measurements/queue-prevention"
exe="/Users/adalundhe/.cache/kiln-offline-state-fix-20261110/red-target/debug/deps/kiln-a2cd230deb814ff5"
wanted=["accepted_queue_edit_reserves_growth_and_journals_before_adapter_mutation","accepted_queue_shrink_grants_retry_credit_only_after_cas_and_actual_owner_apply","attachment_materialization_is_all_or_error_and_never_clobbers_completed_input","deferred_bodies_retain_the_complete_compact_queue_index","the_local_queue_index_is_bounded_and_marks_its_unseen_suffix","a_partial_index_revision_wakes_waiters_even_when_its_count_is_unchanged","status_is_only_explicit_and_does_not_reuse_the_publication_key","a_wait_is_a_change_and_a_result_a_read","cp_t3_2_agent_messages_steer_a_busy_peer_without_building_a_backlog","agents_send_twice_with_one_key_is_one_prompt_and_both_get_its_answer","a_keyed_send_retried_after_its_sender_moved_is_the_same_message"]
listed=subprocess.check_output([exe,"--list"],text=True);names=[line.split(": test")[0] for line in listed.splitlines() if ": test" in line]
selected=[n for n in names if n.split("::")[-1] in wanted]
assert len(selected)==len(wanted),(selected,wanted)
(out/"additional-caller-selection.json").write_text(json.dumps(selected,indent=2)+"\n")
with (out/"additional-caller-tests.log").open("wb") as f:r=subprocess.run(["nice","-n","10",exe,*selected,"--exact","--test-threads=1"],cwd=root,stdout=f,stderr=subprocess.STDOUT)
print(json.dumps({"count":len(selected),"exit_code":r.returncode,"log":str(out/"additional-caller-tests.log")}));raise SystemExit(r.returncode)
