use super::*;
use crate::encoder::caps::Probe as Found;

#[test]
fn nv12_upload_pads_with_the_last_column_and_row() {
    let (y, u, v) = ([1, 2, 3, 4, 5, 6], [7, 8], [9, 10]);
    let f = FrameView {
        width: 3,
        height: 2,
        y: &y,
        u: &u,
        v: &v,
        stride_y: 3,
        stride_uv: 2,
    };
    let mut dst = vec![0u8; 64];
    write_nv12(&mut dst, [5, 6], [0, 40], (4, 4), &f).unwrap();
    assert_eq!(&dst[0..4], &[1, 2, 3, 3]);
    assert_eq!(&dst[5..9], &[4, 5, 6, 6]);
    assert_eq!(&dst[15..19], &[4, 5, 6, 6]);
    assert_eq!(&dst[40..44], &[7, 9, 8, 10]);
    assert_eq!(&dst[46..50], &[7, 9, 8, 10]);
    let mut small = vec![0u8; 20];
    assert!(write_nv12(&mut small, [5, 6], [0, 40], (4, 4), &f).is_err());
    assert!(write_nv12(&mut dst, [5, 6], [0, 40], (2, 4), &f).is_err());
}

#[test]
fn preflight_runs_a_child_once_per_session_and_device() {
    let node = "/dev/dri/renderD128";
    let mut c = sample_config(Backend::Vaapi, Codec::Hevc, 1234, 567, 30);
    c.rate = RateConfig::new(26.5, 0, 24);
    let s = spec(&c, node);
    assert_eq!(s, "hevc:1234x567@30,0,0,24,26.5,/dev/dri/renderD128");
    let (back, n) = parse_spec(Backend::Vaapi, &s).unwrap();
    assert_eq!(
        (back.backend, back.codec, back.width, back.height, back.fps),
        (Backend::Vaapi, Codec::Hevc, 1234, 567, 30)
    );
    assert_eq!(back.rate, c.rate, "the child opens the same rate control");
    assert_eq!(n, node);
    assert!(parse_spec(Backend::Vaapi, "vp9:1x1@1,0,0,1,1,/dev/x").is_none());
    assert!(parse_spec(Backend::Vaapi, "h264:0x1@1,0,0,1,1,/dev/x").is_none());
    assert!(parse_spec(Backend::Vaapi, "h264:1x1@1,0,0,1,1,").is_none());
    let mut runs = 0;
    for kbps in [0, 0] {
        c.rate.max_kbps = kbps;
        let r = preflight(&c, node, |s| {
            runs += 1;
            assert!(s.starts_with("hevc:1234x567@30,0,"));
            Some(Found::failed(Backend::Vaapi, "probe exited with signal 11"))
        });
        assert!(r.unwrap_err().0.contains("signal 11"));
    }
    assert_eq!(runs, 1, "a crash is remembered");
    c.rate = RateConfig::new(26.5, 9000, 24);
    assert!(
        preflight(&c, node, |_| Some(Found::default())).is_ok(),
        "capped is another session"
    );
    c.rate.max_kbps = 4000;
    assert!(
        preflight(&c, node, |_| unreachable!()).is_ok(),
        "the bitrate itself is not"
    );
    c.rate.fps = 60;
    assert!(
        preflight(&c, node, |_| Some(Found::failed(Backend::Vaapi, "crash"))).is_err(),
        "the rate's frame rate is"
    );
    c.rate.fps = 24;
    assert!(
        preflight(&c, "/dev/dri/renderD129", |_| Some(Found::failed(
            Backend::Vaapi,
            "hang"
        )))
        .is_err(),
        "each device is pre-flighted"
    );
    c.backend = Backend::Nvenc;
    assert!(
        preflight(&c, node, |_| Some(Found::failed(Backend::Nvenc, "crash"))).is_err(),
        "each backend is pre-flighted"
    );
    let h264 = sample_config(Backend::Vaapi, Codec::H264, 99, 99, 30);
    assert!(preflight(&h264, node, |_| None).is_ok());
}
