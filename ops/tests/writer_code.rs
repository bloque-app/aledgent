// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Which program the core's writer runs, as the exporter shows it (`T-037-23`; `REQ-RT-16`,
//! `REQ-RT-20`, `NFR-11`): `aledgent_writer_generation`, labelled with the code version in force,
//! and `aledgent_writer_upgrade_scheduled_input`, present only while an upgrade is scheduled; and
//! the signals' field that carries them, which an older reader ignores and a newer reader finds
//! absent from an older core.

use ingress::query::{ScheduledUpgradeBody, SignalsBody, WriterCodeBody};
use ops::signals::{Snapshot, parse, render};

const G: &str = "a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebf";
const H: &str = "c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf";

fn with(writer_code: Option<WriterCodeBody>) -> String {
    render(&Snapshot {
        core: Some(SignalsBody {
            writer_code,
            ..SignalsBody::default()
        }),
        ..Snapshot::default()
    })
}

/// The generation is one series, labelled with the first eight bytes of the version in force; the
/// scheduled upgrade is another, labelled with the version it installs, and only while it is.
#[test]
fn t037_23_the_exposition_shows_the_generation_and_the_scheduled_upgrade() {
    let quiet = with(Some(WriterCodeBody {
        code: G.into(),
        generation: 0,
        upgrade: None,
    }));
    let gen_line = r#"aledgent_writer_generation{writer="core",code="a0a1a2a3a4a5a6a7"} 0"#;
    for line in ["# TYPE aledgent_writer_generation gauge", gen_line] {
        assert!(quiet.lines().any(|l| l == line), "{line}:\n{quiet}");
    }
    assert!(
        !quiet.contains("aledgent_writer_upgrade_scheduled_input"),
        "{quiet}"
    );
    let busy = with(Some(WriterCodeBody {
        code: G.into(),
        generation: 3,
        upgrade: Some(ScheduledUpgradeBody {
            code: H.into(),
            from: 70_000,
        }),
    }));
    for line in [
        r#"aledgent_writer_generation{writer="core",code="a0a1a2a3a4a5a6a7"} 3"#,
        r#"aledgent_writer_upgrade_scheduled_input{writer="core",code="c0c1c2c3c4c5c6c7"} 70000"#,
    ] {
        assert!(busy.lines().any(|l| l == line), "{line}:\n{busy}");
    }
    // One series each: the cardinality is the version in force and the one scheduled.
    let samples = parse(&busy);
    let count = |name: &str| samples.iter().filter(|(n, _, _)| n == name).count();
    assert_eq!(count("aledgent_writer_generation"), 1);
    assert_eq!(count("aledgent_writer_upgrade_scheduled_input"), 1);
    // A core that does not report its code exports neither.
    let none = with(None);
    assert!(!none.contains("aledgent_writer_generation"), "{none}");
}

/// Wire compatibility, both ways (`T-037-23`): a body from an older core has no field and reads as
/// not reported; a body with the field is read by a reader that does not know it (here, one that
/// also meets a field of a later version), and the field is left off a body that has none.
#[test]
fn t037_23_the_signals_field_is_optional_in_both_directions() {
    // What a core without the field sends: the body as it is without it.
    let mut older = serde_json::to_value(SignalsBody::default()).unwrap();
    older.as_object_mut().unwrap().remove("writer_code");
    let s: SignalsBody = serde_json::from_value(older).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(s.writer_code, None);
    let body = SignalsBody {
        writer_code: Some(WriterCodeBody {
            code: G.into(),
            generation: 2,
            upgrade: None,
        }),
        ..SignalsBody::default()
    };
    let mut json = serde_json::to_value(&body).unwrap();
    assert_eq!(
        json["writer_code"],
        serde_json::json!({ "code": G, "generation": 2 }),
        "no `upgrade` key while none is scheduled"
    );
    // A reader that knows nothing of `writer_code` is a reader that ignores unknown fields: shown
    // by reading the body with a field no version has.
    json["a_later_field"] = serde_json::json!(1);
    let back: SignalsBody = serde_json::from_value(json).unwrap();
    assert_eq!(back.writer_code, body.writer_code);
    let plain = serde_json::to_value(SignalsBody::default()).unwrap();
    assert!(plain.get("writer_code").is_none(), "{plain}");
}
