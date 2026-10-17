//! Golden test vectors shared with the TypeScript viewer (`viewer/src/proto/*.test.ts`).
//! Regenerate with: `UPDATE_VECTORS=1 cargo test -p rd-proto --test vectors`.

use rd_proto::bin::*;
use serde_json::{Value, json};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn vectors() -> Vec<Value> {
    let mut v = Vec::new();
    let vh = VideoHeader {
        flags: video_flags::KEY | video_flags::FIRST_OF_EPOCH,
        epoch: 3,
        seq: 42,
        capture_us: 1_234_567_890_123,
        encode_us: 9000,
    };
    v.push(json!({"name":"video","hex":hex(&vh.to_message(&[0,0,0,1,0x65])),"fields":{
        "type":"video","flags":vh.flags,"epoch":3,"seq":42,"capture_us":"1234567890123","encode_us":9000,"payload_hex":"0000000165"}}));
    let ah = AudioHeader {
        flags: 0,
        stream: 1,
        seq: 7,
        capture_us: 9_007_199_254_740_993,
    };
    v.push(json!({"name":"audio","hex":hex(&ah.to_message(&[0xfc,0xff])),"fields":{
        "type":"audio","flags":0,"stream":1,"seq":7,"capture_us":"9007199254740993","payload_hex":"fcff"}}));
    let inputs = [
        (
            "mouse_abs",
            InputEvent::MouseAbs {
                buttons: buttons::LEFT | buttons::RIGHT,
                x: 65535,
                y: 1,
            },
            json!({"type":"mouse_abs","buttons":5,"x":65535,"y":1}),
        ),
        (
            "mouse_rel",
            InputEvent::MouseRel { dx: -5, dy: 300 },
            json!({"type":"mouse_rel","dx":-5,"dy":300}),
        ),
        (
            "mouse_btn",
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: true,
                x: 100,
                y: 200,
            },
            json!({"type":"mouse_btn","button":2,"down":true,"x":100,"y":200}),
        ),
        (
            "wheel",
            InputEvent::Wheel {
                mode: WheelMode::Line,
                dx: 0,
                dy: -3,
                phase: ScrollPhase::None,
            },
            json!({"type":"wheel","mode":1,"dx":0,"dy":-3,"phase":0}),
        ),
        (
            "key",
            InputEvent::Key {
                down: true,
                repeat: true,
                hid: 0x0007_0004,
            },
            json!({"type":"key","down":true,"repeat":true,"hid":458756}),
        ),
        (
            "release_all",
            InputEvent::ReleaseAll,
            json!({"type":"release_all"}),
        ),
    ];
    for (name, ev, fields) in inputs {
        v.push(json!({"name":name,"hex":hex(&ev.to_message()),"fields":fields}));
    }
    let ack = VideoAck {
        flags: ack_flags::DISCARDED,
        epoch: 3,
        seq: 4_000_000_000,
        decode_us: 1500,
        queue_us: 65535,
    };
    v.push(json!({"name":"video_ack","hex":hex(&ack.to_message()),"fields":{
        "type":"video_ack","flags":1,"epoch":3,"seq":4000000000u32,"decode_us":1500,"queue_us":65535}}));
    let ping = Ping {
        id: 9,
        t_origin_us: 123_456_789,
    };
    v.push(json!({"name":"ping","hex":hex(&ping.to_message()),"fields":{"type":"ping","id":9,"t_origin_us":"123456789"}}));
    let pong = ping.pong(987_654_321_000);
    v.push(
        json!({"name":"pong","hex":hex(&pong.to_message()),"fields":{
        "type":"pong","id":9,"t_origin_us":"123456789","t_reply_us":"987654321000"}}),
    );
    v.push(json!({"name":"probe_up","hex":hex(&probe_up_message(&[1,2,3])),"fields":{"type":"probe_up","payload_hex":"010203"}}));
    v
}

#[test]
fn golden_vectors() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testvectors/binary.json");
    let expected = serde_json::to_string_pretty(&vectors()).unwrap() + "\n";
    if std::env::var_os("UPDATE_VECTORS").is_some() {
        std::fs::write(path, &expected).unwrap();
        return;
    }
    let actual = std::fs::read_to_string(path)
        .expect("testvectors/binary.json missing; run with UPDATE_VECTORS=1");
    assert_eq!(
        actual, expected,
        "golden vectors changed; regenerate with UPDATE_VECTORS=1 and update the TS side"
    );
    // Every vector must parse back.
    for vec in vectors() {
        let h = vec["hex"].as_str().unwrap();
        let bytes: Vec<u8> = (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect();
        assert!(
            !matches!(parse(&bytes).unwrap(), BinMsg::Unknown(_)),
            "{}",
            vec["name"]
        );
    }
}
