//! JSON payloads between the server and CI runners (docs/ci.md §2.5): `AssignmentV2` in
//! `ServerMsg::AssignV2` and `JobEventV2` in `AgentMsg::JobEvent`. Plain data, so both sides
//! (and every version of either) agree on one definition; W06 fills them in.
