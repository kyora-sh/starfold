//! `/stats` shows each Quick Sync entry the mode of the sessions that serve it now, LowPower
//! off or on, from fake helpers (shell scripts that print what a helper would) opened on
//! this process's policy.

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

fn open(codec: Codec, low_power: bool) -> Remote {
    let mut c = sample_config(Backend::Qsv, codec, 64, 48, 30);
    c.rate = RateConfig::new(24.0, 2000, 30);
    Remote::open(&POLICY, helper(low_power), &c, 1).unwrap()
}

/// The `low_power` of `/stats`' H.264 and HEVC Quick Sync entries, and of a software one.
fn shown() -> [serde_json::Value; 3] {
    let snapshot = serde_json::json!({
        "mode": "qsv",
        "chosen": [
            {"codec": "h264", "backend": "qsv", "hardware": true},
            {"codec": "hevc", "backend": "qsv", "hardware": true},
            {"codec": "av1", "backend": "svt", "hardware": false},
        ],
        "failed": [],
    });
    let e = with_live(&snapshot, &[]);
    [0, 1, 2].map(|i| e["chosen"][i]["low_power"].clone())
}

#[test]
fn stats_show_each_entry_the_mode_of_the_sessions_serving_it() {
    use serde_json::{json, Value};
    let h264 = open(Codec::H264, false);
    let hevc = open(Codec::Hevc, true);
    assert_eq!(shown(), [json!(false), json!(true), Value::Null]);
    drop(hevc);
    assert_eq!(
        shown(),
        [json!(false), Value::Null, Value::Null],
        "the HEVC session's mode leaves with it"
    );
    let other = open(Codec::H264, true);
    assert_eq!(
        shown(),
        [json!([false, true]), Value::Null, Value::Null],
        "two H.264 sessions in different modes show both"
    );
    drop(h264);
    assert_eq!(shown(), [json!(true), Value::Null, Value::Null]);
    drop(other);
    assert_eq!(shown(), [Value::Null, Value::Null, Value::Null]);
}
