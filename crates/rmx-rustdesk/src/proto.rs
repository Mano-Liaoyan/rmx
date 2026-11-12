//! The part of RustDesk's protobuf schema a viewer needs, written out as `prost` messages.
//!
//! Field numbers and types follow RustDesk's `message.proto` and `rendezvous.proto`, which is
//! all the wire depends on. Messages and fields this client never reads or sends are left out:
//! protobuf skips unknown fields, and an unknown `oneof` member decodes as `None`.

#![allow(clippy::enum_variant_names)]

use bytes::Bytes;
use prost::{Enumeration, Oneof};

/// Encodes a message to the bytes a frame carries.
#[cfg(test)]
pub(crate) fn encode(message: &impl prost::Message) -> Vec<u8> {
    message.encode_to_vec()
}

// ---- message.proto: the session with the peer ----

#[derive(Clone, PartialEq, prost::Message)]
pub struct EncodedVideoFrame {
    #[prost(bytes = "bytes", tag = "1")]
    pub data: Bytes,
    #[prost(bool, tag = "2")]
    pub key: bool,
    #[prost(int64, tag = "3")]
    pub pts: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EncodedVideoFrames {
    #[prost(message, repeated, tag = "1")]
    pub frames: Vec<EncodedVideoFrame>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct VideoFrame {
    #[prost(oneof = "video_frame::Union", tags = "6, 10, 11, 12, 13")]
    pub union: Option<video_frame::Union>,
    #[prost(int32, tag = "14")]
    pub display: i32,
}

pub mod video_frame {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(message, tag = "6")]
        Vp9s(EncodedVideoFrames),
        #[prost(message, tag = "10")]
        H264s(EncodedVideoFrames),
        #[prost(message, tag = "11")]
        H265s(EncodedVideoFrames),
        #[prost(message, tag = "12")]
        Vp8s(EncodedVideoFrames),
        #[prost(message, tag = "13")]
        Av1s(EncodedVideoFrames),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DisplayInfo {
    #[prost(sint32, tag = "1")]
    pub x: i32,
    #[prost(sint32, tag = "2")]
    pub y: i32,
    #[prost(int32, tag = "3")]
    pub width: i32,
    #[prost(int32, tag = "4")]
    pub height: i32,
    #[prost(string, tag = "5")]
    pub name: String,
    #[prost(bool, tag = "6")]
    pub online: bool,
    #[prost(bool, tag = "7")]
    pub cursor_embedded: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LoginRequest {
    /// The ID of the peer being reached.
    #[prost(string, tag = "1")]
    pub username: String,
    /// SHA-256 of the salted password hash and the challenge, or empty to ask for approval.
    #[prost(bytes = "bytes", tag = "2")]
    pub password: Bytes,
    #[prost(string, tag = "4")]
    pub my_id: String,
    #[prost(string, tag = "5")]
    pub my_name: String,
    #[prost(message, optional, tag = "6")]
    pub option: Option<OptionMessage>,
    #[prost(bool, tag = "9")]
    pub video_ack_required: bool,
    #[prost(uint64, tag = "10")]
    pub session_id: u64,
    #[prost(string, tag = "11")]
    pub version: String,
    #[prost(string, tag = "13")]
    pub my_platform: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Auth2Fa {
    #[prost(string, tag = "1")]
    pub code: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PeerInfo {
    #[prost(string, tag = "1")]
    pub username: String,
    #[prost(string, tag = "2")]
    pub hostname: String,
    #[prost(string, tag = "3")]
    pub platform: String,
    #[prost(message, repeated, tag = "4")]
    pub displays: Vec<DisplayInfo>,
    #[prost(int32, tag = "5")]
    pub current_display: i32,
    #[prost(string, tag = "7")]
    pub version: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LoginResponse {
    #[prost(oneof = "login_response::Union", tags = "1, 2")]
    pub union: Option<login_response::Union>,
}

pub mod login_response {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(string, tag = "1")]
        Error(String),
        #[prost(message, tag = "2")]
        PeerInfo(PeerInfo),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MouseEvent {
    /// The event type in the low three bits, the buttons above them.
    #[prost(int32, tag = "1")]
    pub mask: i32,
    #[prost(sint32, tag = "2")]
    pub x: i32,
    #[prost(sint32, tag = "3")]
    pub y: i32,
    #[prost(enumeration = "ControlKey", repeated, tag = "4")]
    pub modifiers: Vec<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum KeyboardMode {
    Legacy = 0,
    Map = 1,
    Translate = 2,
    Auto = 3,
}

/// The keys RustDesk names rather than numbers. Only those this client sends are listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ControlKey {
    Unknown = 0,
    Alt = 1,
    Control = 4,
    Meta = 23,
    Shift = 29,
    Pause = 46,
    RShift = 73,
    RControl = 74,
    RAlt = 75,
    CtrlAltDel = 100,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyEvent {
    #[prost(bool, tag = "1")]
    pub down: bool,
    #[prost(bool, tag = "2")]
    pub press: bool,
    #[prost(oneof = "key_event::Union", tags = "3, 4, 5, 6")]
    pub union: Option<key_event::Union>,
    #[prost(enumeration = "ControlKey", repeated, tag = "8")]
    pub modifiers: Vec<i32>,
    #[prost(enumeration = "KeyboardMode", tag = "9")]
    pub mode: i32,
}

pub mod key_event {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(enumeration = "ControlKey", tag = "3")]
        ControlKey(i32),
        /// In map mode, the position code of the key on the peer's platform.
        #[prost(uint32, tag = "4")]
        Chr(u32),
        #[prost(uint32, tag = "5")]
        Unicode(u32),
        #[prost(string, tag = "6")]
        Seq(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Hash {
    #[prost(string, tag = "1")]
    pub salt: String,
    #[prost(string, tag = "2")]
    pub challenge: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ClipboardFormat {
    Text = 0,
    Rtf = 1,
    Html = 2,
    ImageRgba = 21,
    ImagePng = 22,
    ImageSvg = 23,
    Special = 31,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Clipboard {
    /// The content is zstd compressed.
    #[prost(bool, tag = "1")]
    pub compress: bool,
    #[prost(bytes = "bytes", tag = "2")]
    pub content: Bytes,
    #[prost(int32, tag = "3")]
    pub width: i32,
    #[prost(int32, tag = "4")]
    pub height: i32,
    #[prost(enumeration = "ClipboardFormat", tag = "5")]
    pub format: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MultiClipboards {
    #[prost(message, repeated, tag = "1")]
    pub clipboards: Vec<Clipboard>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Resolution {
    #[prost(int32, tag = "1")]
    pub width: i32,
    #[prost(int32, tag = "2")]
    pub height: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DisplayResolution {
    #[prost(int32, tag = "1")]
    pub display: i32,
    #[prost(message, optional, tag = "2")]
    pub resolution: Option<Resolution>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SwitchDisplay {
    #[prost(int32, tag = "1")]
    pub display: i32,
    #[prost(sint32, tag = "2")]
    pub x: i32,
    #[prost(sint32, tag = "3")]
    pub y: i32,
    #[prost(int32, tag = "4")]
    pub width: i32,
    #[prost(int32, tag = "5")]
    pub height: i32,
    #[prost(bool, tag = "6")]
    pub cursor_embedded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ImageQuality {
    NotSet = 0,
    Low = 2,
    Balanced = 3,
    Best = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum PreferCodec {
    Auto = 0,
    Vp9 = 1,
    H264 = 2,
    H265 = 3,
    Vp8 = 4,
    Av1 = 5,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SupportedDecoding {
    #[prost(int32, tag = "1")]
    pub ability_vp9: i32,
    #[prost(int32, tag = "2")]
    pub ability_h264: i32,
    #[prost(int32, tag = "3")]
    pub ability_h265: i32,
    #[prost(enumeration = "PreferCodec", tag = "4")]
    pub prefer: i32,
    #[prost(int32, tag = "5")]
    pub ability_vp8: i32,
    #[prost(int32, tag = "6")]
    pub ability_av1: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum BoolOption {
    NotSet = 0,
    No = 1,
    Yes = 2,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OptionMessage {
    #[prost(enumeration = "ImageQuality", tag = "1")]
    pub image_quality: i32,
    #[prost(enumeration = "BoolOption", tag = "3")]
    pub show_remote_cursor: i32,
    #[prost(enumeration = "BoolOption", tag = "7")]
    pub disable_audio: i32,
    #[prost(enumeration = "BoolOption", tag = "8")]
    pub disable_clipboard: i32,
    #[prost(message, optional, tag = "10")]
    pub supported_decoding: Option<SupportedDecoding>,
    #[prost(int32, tag = "11")]
    pub custom_fps: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TestDelay {
    #[prost(int64, tag = "1")]
    pub time: i64,
    #[prost(bool, tag = "2")]
    pub from_client: bool,
    #[prost(uint32, tag = "3")]
    pub last_delay: u32,
    #[prost(uint32, tag = "4")]
    pub target_bitrate: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PublicKey {
    #[prost(bytes = "bytes", tag = "1")]
    pub asymmetric_value: Bytes,
    #[prost(bytes = "bytes", tag = "2")]
    pub symmetric_value: Bytes,
    #[prost(uint32, tag = "3")]
    pub kx_version: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SignedId {
    #[prost(bytes = "bytes", tag = "1")]
    pub id: Bytes,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MessageBox {
    #[prost(string, tag = "1")]
    pub msgtype: String,
    #[prost(string, tag = "2")]
    pub title: String,
    #[prost(string, tag = "3")]
    pub text: String,
    #[prost(string, tag = "4")]
    pub link: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Misc {
    #[prost(oneof = "misc::Union", tags = "5, 7, 9, 10, 24, 36")]
    pub union: Option<misc::Union>,
}

pub mod misc {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(message, tag = "5")]
        SwitchDisplay(SwitchDisplay),
        #[prost(message, tag = "7")]
        Option(OptionMessage),
        #[prost(string, tag = "9")]
        CloseReason(String),
        #[prost(bool, tag = "10")]
        RefreshVideo(bool),
        /// For peers before 1.2.4.
        #[prost(message, tag = "24")]
        ChangeResolution(Resolution),
        #[prost(message, tag = "36")]
        ChangeDisplayResolution(DisplayResolution),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Message {
    #[prost(
        oneof = "message::Union",
        tags = "3, 4, 5, 6, 7, 8, 9, 10, 15, 16, 19, 21, 25, 27, 28"
    )]
    pub union: Option<message::Union>,
}

pub mod message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(message, tag = "3")]
        SignedId(SignedId),
        #[prost(message, tag = "4")]
        PublicKey(PublicKey),
        #[prost(message, tag = "5")]
        TestDelay(TestDelay),
        #[prost(message, tag = "6")]
        VideoFrame(VideoFrame),
        #[prost(message, tag = "7")]
        LoginRequest(LoginRequest),
        #[prost(message, tag = "8")]
        LoginResponse(LoginResponse),
        #[prost(message, tag = "9")]
        Hash(Hash),
        #[prost(message, tag = "10")]
        MouseEvent(MouseEvent),
        #[prost(message, tag = "15")]
        KeyEvent(KeyEvent),
        #[prost(message, tag = "16")]
        Clipboard(Clipboard),
        #[prost(message, tag = "19")]
        Misc(Misc),
        #[prost(message, tag = "21")]
        MessageBox(MessageBox),
        #[prost(message, tag = "25")]
        PeerInfo(PeerInfo),
        #[prost(message, tag = "27")]
        Auth2Fa(Auth2Fa),
        #[prost(message, tag = "28")]
        MultiClipboards(MultiClipboards),
    }
}

impl Message {
    pub(crate) fn of(union: message::Union) -> Self {
        Self { union: Some(union) }
    }

    pub(crate) fn misc(union: misc::Union) -> Self {
        Self::of(message::Union::Misc(Misc { union: Some(union) }))
    }
}

// ---- rendezvous.proto: finding the peer ----

/// What a signature binds: an ID and the public key that goes with it.
#[derive(Clone, PartialEq, prost::Message)]
pub struct IdPk {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(bytes = "bytes", tag = "2")]
    pub pk: Bytes,
    /// The newest key exchange version the signer speaks.
    #[prost(uint32, tag = "4")]
    pub kx_version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum NatType {
    UnknownNat = 0,
    Asymmetric = 1,
    Symmetric = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ConnType {
    DefaultConn = 0,
    FileTransfer = 1,
    PortForward = 2,
    Rdp = 3,
    ViewCamera = 4,
    Terminal = 5,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PunchHoleRequest {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(enumeration = "NatType", tag = "2")]
    pub nat_type: i32,
    #[prost(string, tag = "3")]
    pub licence_key: String,
    #[prost(enumeration = "ConnType", tag = "4")]
    pub conn_type: i32,
    #[prost(string, tag = "5")]
    pub token: String,
    #[prost(string, tag = "6")]
    pub version: String,
    #[prost(int32, tag = "7")]
    pub udp_port: i32,
    #[prost(bool, tag = "8")]
    pub force_relay: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum PunchFailure {
    IdNotExist = 0,
    Offline = 2,
    LicenseMismatch = 3,
    LicenseOveruse = 4,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PunchHoleResponse {
    #[prost(bytes = "bytes", tag = "1")]
    pub socket_addr: Bytes,
    #[prost(bytes = "bytes", tag = "2")]
    pub pk: Bytes,
    #[prost(enumeration = "PunchFailure", tag = "3")]
    pub failure: i32,
    #[prost(string, tag = "4")]
    pub relay_server: String,
    #[prost(oneof = "punch_hole_response::Union", tags = "5, 6")]
    pub union: Option<punch_hole_response::Union>,
    #[prost(string, tag = "7")]
    pub other_failure: String,
}

pub mod punch_hole_response {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(enumeration = "NatType", tag = "5")]
        NatType(i32),
        #[prost(bool, tag = "6")]
        IsLocal(bool),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestRelay {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub uuid: String,
    #[prost(string, tag = "4")]
    pub relay_server: String,
    #[prost(bool, tag = "5")]
    pub secure: bool,
    #[prost(string, tag = "6")]
    pub licence_key: String,
    #[prost(enumeration = "ConnType", tag = "7")]
    pub conn_type: i32,
    #[prost(string, tag = "8")]
    pub token: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RelayResponse {
    #[prost(string, tag = "2")]
    pub uuid: String,
    #[prost(string, tag = "3")]
    pub relay_server: String,
    #[prost(oneof = "relay_response::Union", tags = "4, 5")]
    pub union: Option<relay_response::Union>,
    #[prost(string, tag = "6")]
    pub refuse_reason: String,
}

pub mod relay_response {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(string, tag = "4")]
        Id(String),
        #[prost(bytes = "bytes", tag = "5")]
        Pk(Bytes),
    }
}

impl RelayResponse {
    /// The peer's ID and key as the rendezvous server signed them, if it sent them.
    pub(crate) fn signed_pk(&self) -> Bytes {
        match &self.union {
            Some(relay_response::Union::Pk(pk)) => pk.clone(),
            _ => Bytes::new(),
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyExchange {
    #[prost(bytes = "bytes", repeated, tag = "1")]
    pub keys: Vec<Bytes>,
    #[prost(uint32, tag = "2")]
    pub version: u32,
    #[prost(bytes = "bytes", tag = "3")]
    pub signed_params: Bytes,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RendezvousMessage {
    #[prost(oneof = "rendezvous_message::Union", tags = "8, 11, 18, 19, 25")]
    pub union: Option<rendezvous_message::Union>,
}

pub mod rendezvous_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Union {
        #[prost(message, tag = "8")]
        PunchHoleRequest(PunchHoleRequest),
        #[prost(message, tag = "11")]
        PunchHoleResponse(PunchHoleResponse),
        #[prost(message, tag = "18")]
        RequestRelay(RequestRelay),
        #[prost(message, tag = "19")]
        RelayResponse(RelayResponse),
        #[prost(message, tag = "25")]
        KeyExchange(KeyExchange),
    }
}

impl RendezvousMessage {
    pub(crate) fn of(union: rendezvous_message::Union) -> Self {
        Self { union: Some(union) }
    }
}

/// Decodes `bytes` as `M`, or `None` if they are not one.
pub(crate) fn decode<M: prost::Message + Default>(bytes: &[u8]) -> Option<M> {
    M::decode(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_request_round_trips() {
        let request = LoginRequest {
            username: "123456789".into(),
            password: Bytes::from_static(&[1, 2, 3]),
            my_id: "rmx".into(),
            session_id: 7,
            option: Some(OptionMessage {
                supported_decoding: Some(SupportedDecoding {
                    ability_vp9: 1,
                    prefer: PreferCodec::Vp9 as i32,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let message = Message::of(message::Union::LoginRequest(request.clone()));
        let decoded: Message = decode(&encode(&message)).unwrap();
        assert_eq!(decoded.union, Some(message::Union::LoginRequest(request)));
    }

    #[test]
    fn an_unknown_union_member_decodes_as_none() {
        // Field 11 of Message, an audio frame, is not part of this schema.
        let bytes = [11 << 3 | 2, 0];
        let decoded: Message = decode(&bytes).unwrap();
        assert_eq!(decoded.union, None);
    }

    #[test]
    fn field_numbers_match_the_wire() {
        // A SignedId of one byte: Message field 3, SignedId field 1.
        let message =
            Message::of(message::Union::SignedId(SignedId { id: Bytes::from_static(b"x") }));
        assert_eq!(encode(&message), [0x1a, 3, 0x0a, 1, b'x']);
        // A PunchHoleRequest for "1": RendezvousMessage field 8.
        let request =
            RendezvousMessage::of(rendezvous_message::Union::PunchHoleRequest(PunchHoleRequest {
                id: "1".into(),
                ..Default::default()
            }));
        assert_eq!(encode(&request), [0x42, 3, 0x0a, 1, b'1']);
    }
}
