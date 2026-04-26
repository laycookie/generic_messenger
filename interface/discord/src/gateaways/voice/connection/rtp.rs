use std::{
    num::Wrapping,
    ops::{Add, AddAssign, Sub, SubAssign},
};

use num_enum::TryFromPrimitive;
use pnet_macros::packet;
use pnet_macros_support::{
    packet::PrimitiveValues,
    types::{u1, u2, u4, u7, u16be, u32be},
};

pub type Ssrc = u32;

#[derive(Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
pub(super) enum DiscordPacketType {
    Voice = 0x78,
    /// RTCP Sender Report
    RtcpSenderReport = 200,
    /// RTCP Receiver Report
    RtcpReceiverReport = 201,
}

// === RTP definitions ===

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct WrapU16(pub Wrapping<u16>);

impl WrapU16 {
    #[must_use]
    pub fn new(v: u16be) -> Self {
        Self(Wrapping(v))
    }
}

impl From<WrapU16> for u16 {
    fn from(val: WrapU16) -> Self {
        (val.0).0
    }
}

impl From<u16> for WrapU16 {
    fn from(val: u16) -> Self {
        WrapU16(Wrapping(val))
    }
}

impl PrimitiveValues for WrapU16 {
    type T = (u16be,);

    fn to_primitive_values(&self) -> Self::T {
        ((*self).into(),)
    }
}

impl Add<u16> for WrapU16 {
    type Output = Self;

    fn add(self, other: u16) -> Self::Output {
        WrapU16(self.0 + Wrapping(other))
    }
}

impl AddAssign<u16> for WrapU16 {
    fn add_assign(&mut self, other: u16) {
        self.0 += Wrapping(other);
    }
}

impl Sub<u16> for WrapU16 {
    type Output = Self;

    fn sub(self, other: u16) -> Self::Output {
        WrapU16(self.0 - Wrapping(other))
    }
}

impl SubAssign<u16> for WrapU16 {
    fn sub_assign(&mut self, other: u16) {
        self.0 -= Wrapping(other);
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct WrapU32(pub Wrapping<u32>);

impl WrapU32 {
    #[must_use]
    pub fn new(v: u32be) -> Self {
        Self(Wrapping(v))
    }
}

impl From<WrapU32> for u32 {
    fn from(val: WrapU32) -> Self {
        (val.0).0
    }
}

impl From<u32> for WrapU32 {
    fn from(val: u32) -> Self {
        WrapU32(Wrapping(val))
    }
}

impl PrimitiveValues for WrapU32 {
    type T = (u32be,);

    fn to_primitive_values(&self) -> Self::T {
        ((*self).into(),)
    }
}

impl Add<u32> for WrapU32 {
    type Output = Self;

    fn add(self, other: u32) -> Self::Output {
        WrapU32(self.0 + Wrapping(other))
    }
}

impl AddAssign<u32> for WrapU32 {
    fn add_assign(&mut self, other: u32) {
        self.0 += Wrapping(other);
    }
}

impl Sub<u32> for WrapU32 {
    type Output = Self;

    fn sub(self, other: u32) -> Self::Output {
        WrapU32(self.0 - Wrapping(other))
    }
}

impl SubAssign<u32> for WrapU32 {
    fn sub_assign(&mut self, other: u32) {
        self.0 -= Wrapping(other);
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// [IANA page]: https://www.iana.org/assignments/rtp-parameters/rtp-parameters.xhtml#rtp-parameters-1
pub enum RtpType {
    /// Code 0.
    Pcmu,
    /// Code 3.
    Gsm,
    /// Code 4.
    G723,
    /// Code 5--6, 16--17.
    Dvi4(u8),
    /// Code 7.
    Lpc,
    /// Code 8.
    Pcma,
    /// Code 9.
    G722,
    /// Code 10.
    L16Stereo,
    /// Code 11.
    L16Mono,
    /// Code 12.
    Qcelp,
    /// Code 13.
    Cn,
    /// Code 14.
    Mpa,
    /// Code 15.
    G728,
    /// Code 18.
    G729,
    /// Code 25.
    CelB,
    /// Code 26.
    Jpeg,
    /// Code 28.
    Nv,
    /// Code 31.
    H261,
    /// Code 32.
    Mpv,
    /// Code 33.
    Mp2t,
    /// Code 34.
    H263,
    /// Dynamically assigned payload type (codes >= 96).
    Dynamic(u8),
    /// Reserved payload types, typically to mitigate RTCP packet type collisions (1--2, 19, 72--76).
    Reserved(u8),
    /// Unassigned payload type (all remaining < 128).
    Unassigned(u8),
    /// Code point too high for u7: application error?
    Illegal(u8),
}
impl RtpType {
    #[must_use]
    pub fn new(val: u7) -> Self {
        match val {
            0 => Self::Pcmu,
            3 => Self::Gsm,
            4 => Self::G723,
            5 | 6 | 16 | 17 => Self::Dvi4(val),
            7 => Self::Lpc,
            8 => Self::Pcma,
            9 => Self::G722,
            10 => Self::L16Stereo,
            11 => Self::L16Mono,
            12 => Self::Qcelp,
            13 => Self::Cn,
            14 => Self::Mpa,
            15 => Self::G728,
            18 => Self::G729,
            25 => Self::CelB,
            26 => Self::Jpeg,
            28 => Self::Nv,
            31 => Self::H261,
            32 => Self::Mpv,
            33 => Self::Mp2t,
            34 => Self::H263,
            1..=2 | 19 | 72..=76 => Self::Reserved(val),
            96..=127 => Self::Dynamic(val),
            128..=255 => Self::Illegal(val),
            _ => Self::Unassigned(val),
        }
    }
}

impl PrimitiveValues for RtpType {
    type T = (u7,);

    fn to_primitive_values(&self) -> Self::T {
        match self {
            Self::Pcmu => (0,),
            Self::Gsm => (3,),
            Self::G723 => (4,),
            Self::Lpc => (7,),
            Self::Pcma => (8,),
            Self::G722 => (9,),
            Self::L16Stereo => (10,),
            Self::L16Mono => (11,),
            Self::Qcelp => (12,),
            Self::Cn => (13,),
            Self::Mpa => (14,),
            Self::G728 => (15,),
            Self::G729 => (18,),
            Self::CelB => (25,),
            Self::Jpeg => (26,),
            Self::Nv => (28,),
            Self::H261 => (31,),
            Self::Mpv => (32,),
            Self::Mp2t => (33,),
            Self::H263 => (34,),

            Self::Dvi4(val)
            | Self::Dynamic(val)
            | Self::Unassigned(val)
            | Self::Reserved(val)
            | Self::Illegal(val) => (*val,),
        }
    }
}

/// [Real-time Transport Protocol]: https://tools.ietf.org/html/rfc3550
#[packet]
#[derive(Eq, PartialEq)]
pub struct Rtp {
    pub version: u2,
    pub padding: u1,
    pub extension: u1,
    pub csrc_count: u4,
    pub marker: u1,
    #[construct_with(u7)]
    pub payload_type: RtpType,
    #[construct_with(u16be)]
    pub sequence: WrapU16,
    #[construct_with(u32be)]
    pub timestamp: WrapU32,
    pub ssrc: u32be,
    #[length = "csrc_count"]
    pub csrc_list: Vec<u32be>,
    #[payload]
    pub payload: Vec<u8>,
}
