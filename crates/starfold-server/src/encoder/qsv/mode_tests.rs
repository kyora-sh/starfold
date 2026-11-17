//! `/stats` shows the mode a Quick Sync session opened with, LowPower off or on, from a
//! fake helper (a shell script that prints what a helper would) opened on this process's
//! policy.

use super::proc::{self, NotStarted, Pipes, Proc};
use super::proto::Reply;
use super::remote::{Remote, POLICY};
use crate::encoder::hw::sample_config;
use crate::encoder::{with_live, Backend, Codec, EncoderConfig, RateConfig};
use std::process::Command;

fn helper(low_power: bool) -> impl FnOnce() -> Result<(Proc, Pipes), NotStarted> {
    let r = Reply::Opened {
        low_power,
        name: "fake-gen".into(),
    };
    let octal: String = r.bytes().iter().map(|b| format!("\\{b:03o}")).collect();
    move || {
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!("printf '{octal}'; sleep 30"));
        proc::spawn(c)
    }
}

fn cfg() -> EncoderConfig {
    let mut c = sample_config(Backend::Qsv, Codec::H264, 64, 48, 30);
    c.rate = RateConfig::new(24.0, 2000, 30);
    c
}

#[test]
fn stats_show_the_mode_a_quick_sync_session_opened_with() {
    let snapshot = serde_json::json!({
        "mode": "qsv",
        "chosen": [
            {"codec": "h264", "backend": "qsv", "hardware": true},
            {"codec": "av1", "backend": "svt", "hardware": false},
        ],
        "failed": [],
    });
    for low_power in [false, true] {
        let enc = Remote::open(&POLICY, helper(low_power), &cfg(), 1).unwrap();
        let e = with_live(&snapshot, &[]);
        assert_eq!(
            e["chosen"][0]["low_power"],
            serde_json::json!(low_power),
            "{e}"
        );
        assert!(e["chosen"][1].get("low_power").is_none(), "{e}");
        drop(enc);
    }
}
