use std::{io, marker::PhantomData};

use async_tungstenite::tungstenite::Message as WebsocketMessage;
use facet::Facet;

use super::{general::GatewayEvent, media};

#[derive(Debug)]
pub(crate) enum GatewayFrameConversionError {
    Control,
    UnexpectedBinary,
}

/// Owned websocket data for one Discord gateway protocol frame.
/// Parsing returns a view borrowing this frame, keeping JSON and binary bodies
/// unmaterialized until a handler requests them.
pub(crate) struct GatewayFrame<Op> {
    message: WebsocketMessage,
    _marker: PhantomData<fn() -> Op>,
}

impl<Op> GatewayFrame<Op> {
    pub(crate) fn text(text: String) -> Self {
        Self {
            message: WebsocketMessage::text(text),
            _marker: PhantomData,
        }
    }

    pub(crate) fn into_message(self) -> WebsocketMessage {
        self.message
    }
}

impl TryFrom<WebsocketMessage> for GatewayFrame<super::general::Opcode> {
    type Error = GatewayFrameConversionError;

    fn try_from(message: WebsocketMessage) -> Result<Self, Self::Error> {
        match message {
            WebsocketMessage::Text(_) => Ok(Self {
                message,
                _marker: PhantomData,
            }),
            WebsocketMessage::Binary(_) => Err(GatewayFrameConversionError::UnexpectedBinary),
            _ => Err(GatewayFrameConversionError::Control),
        }
    }
}

impl TryFrom<WebsocketMessage> for GatewayFrame<media::VoiceOpcode> {
    type Error = GatewayFrameConversionError;

    fn try_from(message: WebsocketMessage) -> Result<Self, Self::Error> {
        match message {
            WebsocketMessage::Text(_) | WebsocketMessage::Binary(_) => Ok(Self {
                message,
                _marker: PhantomData,
            }),
            _ => Err(GatewayFrameConversionError::Control),
        }
    }
}

/// <https://docs.discord.food/gateway/gateway-events#gateway-payload-structure>
#[derive(Facet)]
struct JsonGatewayPayload<'a, Op> {
    op: Op,
    // Keep dispatch event names raw. Discord can add event names without a
    // gateway version bump; decoding the envelope must still preserve `s` so
    // heartbeats/resume state advance even when this client ignores the event.
    t: Option<facet_json::RawJson<'a>>,
    s: Option<u64>,
    d: facet_json::RawJson<'a>,
}

enum GatewayData<'a> {
    Json(facet_json::RawJson<'a>),
    Binary(&'a [u8]),
}

pub(crate) struct GatewayPayload<'a, Op> {
    op: Op,
    event_type: Option<facet_json::RawJson<'a>>,
    sequence: Option<u64>,
    data: GatewayData<'a>,
}

impl<Op> GatewayFrame<Op>
where
    Op: for<'facet> Facet<'facet> + TryFrom<u8>,
{
    pub(super) fn payload(&self) -> Result<GatewayPayload<'_, Op>, io::Error> {
        match &self.message {
            WebsocketMessage::Text(utf8) => {
                let payload: JsonGatewayPayload<'_, Op> = facet_json::from_str_borrowed(utf8)
                    .map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid gateway JSON payload: {err}"),
                        )
                    })?;
                Ok(GatewayPayload {
                    op: payload.op,
                    event_type: payload.t,
                    sequence: payload.s,
                    data: GatewayData::Json(payload.d),
                })
            }
            WebsocketMessage::Binary(bytes) => {
                let frame = media::VoiceBinaryFrame::parse(bytes)?;
                let opcode_byte = frame.opcode as u8;
                let op = Op::try_from(opcode_byte).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("opcode {opcode_byte} is invalid for this gateway"),
                    )
                })?;
                Ok(GatewayPayload {
                    op,
                    event_type: None,
                    sequence: frame.sequence.map(u64::from),
                    data: GatewayData::Binary(frame.payload),
                })
            }
            _ => unreachable!("GatewayFrame conversions reject websocket control frames"),
        }
    }
}

impl<'a, Op> GatewayPayload<'a, Op> {
    pub(super) fn opcode(&self) -> &Op {
        &self.op
    }

    /// Decode a known dispatch event name without making the surrounding
    /// gateway envelope depend on that closed enum. `Some(Err(_))` means the
    /// server sent a syntactically valid but unknown/non-string `t` value.
    pub(super) fn event_type(&self) -> Option<Result<GatewayEvent, io::Error>> {
        self.event_type.as_ref().map(|raw| {
            facet_json::from_str(raw.as_str()).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown gateway event type: {err}"),
                )
            })
        })
    }

    pub(super) fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    pub(super) fn decode<T: Facet<'static>>(&self) -> Result<T, io::Error> {
        match &self.data {
            GatewayData::Json(raw) => facet_json::from_str(raw.as_str()).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid gateway event data: {err}"),
                )
            }),
            GatewayData::Binary(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot decode a binary gateway payload as JSON",
            )),
        }
    }

    pub(super) fn binary(&self) -> Result<&'a [u8], io::Error> {
        match &self.data {
            GatewayData::Binary(bytes) => Ok(bytes),
            GatewayData::Json(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gateway payload is JSON, not binary",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_tungstenite::tungstenite::Bytes;

    #[derive(Facet)]
    struct TestPayload {
        value: u8,
    }

    #[test]
    fn main_frame_decodes_json_data() {
        let message = WebsocketMessage::Text(
            r#"{"op":0,"t":"MESSAGE_DELETE_BULK","s":42,"d":{"value":7}}"#.into(),
        );
        let frame = GatewayFrame::<super::super::general::Opcode>::try_from(message).unwrap();
        let payload = frame.payload().unwrap();

        assert!(matches!(
            payload.opcode(),
            super::super::general::Opcode::Dispatch
        ));
        assert!(matches!(
            payload.event_type(),
            Some(Ok(GatewayEvent::MessageDeleteBulk))
        ));
        assert_eq!(payload.sequence(), Some(42));
        assert_eq!(payload.decode::<TestPayload>().unwrap().value, 7);
    }

    #[test]
    fn main_frame_preserves_unknown_dispatch_sequence() {
        let message = WebsocketMessage::Text(
            r#"{"op":0,"t":"FUTURE_DISPATCH","s":43,"d":{"value":7}}"#.into(),
        );
        let frame = GatewayFrame::<super::super::general::Opcode>::try_from(message).unwrap();
        let payload = frame.payload().unwrap();

        assert_eq!(payload.sequence(), Some(43));
        assert!(matches!(payload.event_type(), Some(Err(_))));
        assert_eq!(payload.decode::<TestPayload>().unwrap().value, 7);
    }

    #[test]
    fn main_frame_rejects_binary_messages() {
        let result = GatewayFrame::<super::super::general::Opcode>::try_from(
            WebsocketMessage::Binary(Bytes::from_static(&[0, 1, 2])),
        );

        assert!(matches!(
            result,
            Err(GatewayFrameConversionError::UnexpectedBinary)
        ));
    }

    #[test]
    fn voice_frame_accepts_text_messages() {
        let message = WebsocketMessage::Text(r#"{"op":8,"d":{"value":9}}"#.into());
        let frame = GatewayFrame::<media::VoiceOpcode>::try_from(message).unwrap();
        let payload = frame.payload().unwrap();

        assert!(matches!(payload.opcode(), media::VoiceOpcode::Hello));
        assert_eq!(payload.decode::<TestPayload>().unwrap().value, 9);
    }

    #[test]
    fn voice_binary_payload_borrows_websocket_bytes() {
        let bytes = Bytes::from_static(&[0, 9, media::VoiceOpcode::MLSProposals as u8, 1, 2, 3]);
        let body_pointer = bytes[3..].as_ptr();
        let frame =
            GatewayFrame::<media::VoiceOpcode>::try_from(WebsocketMessage::Binary(bytes)).unwrap();
        let payload = frame.payload().unwrap();
        let body = payload.binary().unwrap();

        assert!(matches!(payload.opcode(), media::VoiceOpcode::MLSProposals));
        assert_eq!(payload.sequence(), Some(9));
        assert_eq!(body, [1, 2, 3]);
        assert_eq!(body.as_ptr(), body_pointer);
    }

    #[test]
    fn malformed_json_is_rejected_when_payload_is_requested() {
        let message = WebsocketMessage::Text(r#"{"op":0,"d":{"value":7}"#.into());
        let frame = GatewayFrame::<super::super::general::Opcode>::try_from(message).unwrap();

        assert!(frame.payload().is_err());
    }

    #[test]
    fn control_messages_are_not_gateway_frames() {
        let result =
            GatewayFrame::<media::VoiceOpcode>::try_from(WebsocketMessage::Ping(Bytes::new()));

        assert!(matches!(result, Err(GatewayFrameConversionError::Control)));
    }
}
