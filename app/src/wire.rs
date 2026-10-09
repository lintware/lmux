//! Thin helpers over the frozen `lmux-proto` contract. No hand-mirroring: the
//! app uses the real types from the shared crate.

pub use lmux_proto::frame::{Frame, FrameError, KIND_BROWSER_FRAME, KIND_TERM_OUTPUT};
pub use lmux_proto::methods as m;
pub use lmux_proto::*;

use serde_json::{json, Value};

pub fn rpc(id: u64, method: &str, params: Value) -> ClientMsg {
    ClientMsg::Rpc {
        request: RpcRequest { id, method: method.to_owned(), params },
    }
}

pub fn rpc_empty(id: u64, method: &str) -> ClientMsg {
    rpc(id, method, json!({}))
}

pub fn encode(msg: &ClientMsg) -> String {
    serde_json::to_string(msg).expect("ClientMsg is always serialisable")
}

pub fn decode_server(text: &str) -> Option<ServerMsg> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_input_round_trip() {
        let msg = ClientMsg::TermInput { surface: "surface:3".into(), data: "ls\r".into() };
        let text = encode(&msg);
        assert!(text.contains("\"type\":\"term_input\""));
        let back: ClientMsg = serde_json::from_str(&text).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn server_msg_decode() {
        let text = r#"{"type":"welcome","protocol_version":1,"server_version":"mock","host":"h"}"#;
        match decode_server(text) {
            Some(ServerMsg::Welcome { protocol_version, host, .. }) => {
                assert_eq!(protocol_version, 1);
                assert_eq!(host, "h");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tree_decode() {
        let tree = Tree {
            host: "lint".into(),
            folders: vec![Folder { id: "folder:1".into(), name: "ws".into(), parent: None, collapsed: false }],
            workspaces: vec![],
            selected_workspace: None,
        };
        let text = serde_json::to_string(&ServerMsg::Tree { tree: tree.clone() }).unwrap();
        match decode_server(&text) {
            Some(ServerMsg::Tree { tree: t }) => assert_eq!(t, tree),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn frame_round_trip() {
        let f = Frame::TermOutput { surface: "surface:4", data: b"\x1b[31mhi\r\n" };
        let bytes = f.encode().unwrap();
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn browser_frame_decode() {
        let f = Frame::BrowserFrame { surface: "surface:9", seq: 7, width: 1280, height: 800, jpeg: &[0xff, 0xd8] };
        let bytes = f.encode().unwrap();
        match Frame::decode(&bytes).unwrap() {
            Frame::BrowserFrame { seq, width, height, jpeg, .. } => {
                assert_eq!((seq, width, height, jpeg), (7, 1280, 800, &[0xff, 0xd8][..]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rpc_shape() {
        let msg = rpc(7, m::WORKSPACE_SELECT, json!({"workspace": "workspace:1"}));
        let text = encode(&msg);
        assert!(text.contains("\"method\":\"workspace.select\""));
        assert!(text.contains("\"id\":7"));
    }
}
