#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

//! Source hypothesis controls for the actual managed helper Launch validator.
//! Overlay as a standalone hex-core integration test; keep production050 unchanged for original red.
use hex_core::helper::managed::{CpuControl, Launch, RecordKind};
use std::time::Duration;

fn prepared() -> Launch {
    Launch {
        record_kind: RecordKind::Box,
        // The protocol checks the byte budget; the shell separately decodes the actual canonical row.
        record: b"{}\n".to_vec(),
        binary: b"/owned/shards".to_vec(),
        arguments: [
            "run",
            "--kernel",
            "/owned/kernel",
            "--cmdline",
            "console=ttyS0",
            "--host-managed",
        ]
        .map(|word| word.as_bytes().to_vec())
        .into(),
        environment: vec![
            b"SHARDS_CACHE=/owned/cache".to_vec(),
            b"SHARDS_HOME=/owned/home".to_vec(),
        ],
        vsock: b"/owned/vsock".to_vec(),
        cpu: CpuControl {
            cost_id: [3; 32],
            response: Duration::from_nanos(17),
        },
        report_scope: None,
    }
}

/// AC-T7.4.1: a kernel command-line VALUE is not a pre-rendered native report option.
#[test]
fn managed_valued_command_line_preserves_literal_report_flag_words() {
    let control = prepared();
    control.check().unwrap();
    for value in ["--gpu-report", "--gpu-report-scope"] {
        let mut launch = prepared();
        launch.arguments[4] = value.as_bytes().to_vec();
        assert!(launch.check().is_ok(), "literal --cmdline value {value}");
    }
}

/// AC-T7.4.1: a literal '--' as a VALUE cannot hide a later real native report option.
#[test]
fn managed_valued_double_dash_cannot_hide_a_later_pre_rendered_report() {
    let mut launch = prepared();
    launch.arguments[4] = b"--".to_vec();
    launch.check().unwrap();
    launch
        .arguments
        .extend([b"--gpu-report".to_vec(), b"41".to_vec()]);
    assert!(
        launch.check().is_err(),
        "native writer must come only from final native launcher"
    );
}
