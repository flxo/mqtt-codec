use std::io::Cursor;

use bytes::{Buf, Bytes};
use string::{String, TryFrom};

use crate::error::ParseError;
use crate::packet::*;
use crate::proto::*;

use super::{ConnectAckFlags, ConnectFlags, FixedHeader, WILL_QOS_SHIFT};

macro_rules! check_flag {
    ($flags:expr, $flag:expr) => {
        ($flags & $flag.bits()) == $flag.bits()
    };
}

macro_rules! ensure {
    ($cond:expr, $e:expr) => {
        if !($cond) {
            return Err($e);
        }
    };
    ($cond:expr, $fmt:expr, $($arg:tt)+) => {
        if !($cond) {
            return Err($fmt, $($arg)+);
        }
    };
}

pub(crate) fn read_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    match header.packet_type {
        CONNECT => decode_connect_packet(src, header),
        CONNACK => decode_connect_ack_packet(src, header),
        PUBLISH => decode_publish_packet(src, header),
        PUBACK => decode_publish_ack_packet(src, header),
        PUBREC => decode_publish_rec_packet(src, header),
        PUBREL => decode_publish_rel_packet(src, header),
        PUBCOMP => decode_publish_comp_packet(src, header),
        SUBSCRIBE => decode_subscribe_packet(src, header),
        SUBACK => decode_subscribe_ack_packet(src, header),
        UNSUBSCRIBE => decode_unsubscribe_packet(src, header),
        UNSUBACK => decode_unsubscribe_ack_packet(src, header),
        PINGREQ => {
            ensure!(
                header.packet_flags.trailing_zeros() >= 4,
                ParseError::FixedHeaderReservedFlagsMismatch
            );
            Ok(Packet::PingRequest)
        }
        PINGRESP => {
            ensure!(
                header.packet_flags.trailing_zeros() >= 4,
                ParseError::FixedHeaderReservedFlagsMismatch
            );
            Ok(Packet::PingResponse)
        }
        DISCONNECT => {
            ensure!(
                header.packet_flags.trailing_zeros() >= 4,
                ParseError::FixedHeaderReservedFlagsMismatch
            );
            Ok(Packet::Disconnect)
        }
        _ => Err(ParseError::UnsupportedPacketType),
    }
}

pub fn decode_variable_length(src: &[u8]) -> Result<Option<(usize, usize)>, ParseError> {
    if let Some((len, consumed, more)) = src
        .iter()
        .enumerate()
        .scan((0, true), |state, (idx, x)| {
            if !state.1 || idx > 3 {
                return None;
            }
            state.0 += ((x & 0x7F) as usize) << (idx * 7);
            state.1 = x & 0x80 != 0;
            Some((state.0, idx + 1, state.1))
        })
        .last()
    {
        ensure!(!more || consumed < 4, ParseError::InvalidLength);
        return Ok(Some((len, consumed)));
    }

    Ok(None)
}

fn decode_connect_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags.trailing_zeros() >= 4,
        ParseError::FixedHeaderReservedFlagsMismatch
    );

    ensure!(src.remaining() >= 10, ParseError::InvalidLength);
    let len = src.get_u16_be();
    ensure!(
        len == 4 && &src.bytes()[0..4] == b"MQTT",
        ParseError::InvalidProtocol
    );
    src.advance(4);

    let level = src.get_u8();
    ensure!(
        level == DEFAULT_MQTT_LEVEL,
        ParseError::UnsupportedProtocolLevel
    );

    let flags = src.get_u8();
    ensure!((flags & 0x01) == 0, ParseError::ConnectReservedFlagSet);

    let keep_alive = src.get_u16_be();
    let client_id = decode_utf8_str(src)?;

    ensure!(
        !client_id.is_empty() || check_flag!(flags, ConnectFlags::CLEAN_SESSION),
        ParseError::InvalidClientId
    );

    let topic = if check_flag!(flags, ConnectFlags::WILL) {
        Some(decode_utf8_str(src)?)
    } else {
        None
    };
    let message = if check_flag!(flags, ConnectFlags::WILL) {
        Some(decode_length_bytes(src)?)
    } else {
        None
    };
    let username = if check_flag!(flags, ConnectFlags::USERNAME) {
        Some(decode_utf8_str(src)?)
    } else {
        None
    };
    let password = if check_flag!(flags, ConnectFlags::PASSWORD) {
        Some(decode_length_bytes(src)?)
    } else {
        None
    };
    let last_will = if topic.is_some() {
        Some(LastWill {
            qos: QoS::from((flags & ConnectFlags::WILL_QOS.bits()) >> WILL_QOS_SHIFT),
            retain: check_flag!(flags, ConnectFlags::WILL_RETAIN),
            topic: topic.unwrap(),
            message: message.unwrap(),
        })
    } else {
        None
    };

    Ok(Packet::Connect(Connect {
        protocol: Protocol::MQTT(level),
        clean_session: check_flag!(flags, ConnectFlags::CLEAN_SESSION),
        keep_alive,
        client_id,
        last_will,
        username,
        password,
    }))
}

fn decode_connect_ack_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags.trailing_zeros() >= 4,
        ParseError::FixedHeaderReservedFlagsMismatch
    );

    ensure!(src.remaining() >= 2, ParseError::InvalidLength);
    let flags = src.get_u8();
    ensure!(
        (flags & 0b1111_1110) == 0,
        ParseError::ConnAckReservedFlagSet
    );

    let return_code = src.get_u8();
    Ok(Packet::ConnectAck {
        session_present: check_flag!(flags, ConnectAckFlags::SESSION_PRESENT),
        return_code: ConnectCode::from(return_code),
    })
}

fn decode_publish_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    let topic = decode_utf8_str(src)?;
    let qos = QoS::from((header.packet_flags & 0b0110) >> 1);
    let packet_id = if qos == QoS::AtMostOnce {
        None
    } else {
        Some(read_u16(src)?)
    };

    let len = src.remaining();
    let payload = take(src, len);

    Ok(Packet::Publish(Publish {
        dup: (header.packet_flags & 0b1000) == 0b1000,
        qos,
        retain: (header.packet_flags & 0b0001) == 0b0001,
        topic,
        packet_id,
        payload,
    }))
}

fn decode_publish_ack_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags.trailing_zeros() >= 4,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    Ok(Packet::PublishAck {
        packet_id: read_u16(src)?,
    })
}

fn decode_publish_rec_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags.trailing_zeros() >= 4,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    Ok(Packet::PublishAck {
        packet_id: read_u16(src)?,
    })
}

fn decode_publish_rel_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags == 0b0010,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    Ok(Packet::PublishRelease {
        packet_id: read_u16(src)?,
    })
}

fn decode_publish_comp_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags.trailing_zeros() >= 4,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    Ok(Packet::PublishRelease {
        packet_id: read_u16(src)?,
    })
}

fn decode_subscribe_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags == 0b0010,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    let packet_id = read_u16(src)?;
    let mut topic_filters = Vec::new();
    while src.remaining() > 0 {
        let topic = decode_utf8_str(src)?;
        ensure!(src.remaining() >= 1, ParseError::InvalidLength);
        let qos = QoS::from(src.get_u8() & 0x03);
        topic_filters.push((topic, qos));
    }

    Ok(Packet::Subscribe {
        packet_id,
        topic_filters,
    })
}

fn decode_subscribe_ack_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags.trailing_zeros() >= 4,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    let packet_id = read_u16(src)?;
    let status = src
        .iter()
        .map(|code| {
            if code == 0x80 {
                SubscribeReturnCode::Failure
            } else {
                SubscribeReturnCode::Success(QoS::from(code & 0x03))
            }
        })
        .collect();
    Ok(Packet::SubscribeAck { packet_id, status })
}

fn decode_unsubscribe_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags == 0b0010,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    let packet_id = read_u16(src)?;
    let mut topic_filters = Vec::new();
    while src.remaining() > 0 {
        topic_filters.push(decode_utf8_str(src)?);
    }
    Ok(Packet::Unsubscribe {
        packet_id,
        topic_filters,
    })
}

fn decode_unsubscribe_ack_packet(
    src: &mut Cursor<Bytes>,
    header: FixedHeader,
) -> Result<Packet, ParseError> {
    ensure!(
        header.packet_flags.trailing_zeros() >= 4,
        ParseError::FixedHeaderReservedFlagsMismatch
    );
    Ok(Packet::UnsubscribeAck {
        packet_id: read_u16(src)?
    })
}

fn decode_length_bytes(src: &mut Cursor<Bytes>) -> Result<Bytes, ParseError> {
    let len = read_u16(src)? as usize;
    ensure!(src.remaining() >= len, ParseError::InvalidLength);
    Ok(take(src, len))
}

fn decode_utf8_str(src: &mut Cursor<Bytes>) -> Result<String<Bytes>, ParseError> {
    let bytes = decode_length_bytes(src)?;
    Ok(String::try_from(bytes)?)
}

fn take(buf: &mut Cursor<Bytes>, n: usize) -> Bytes {
    let pos = buf.position() as usize;
    let ret = buf.get_ref().slice(pos, pos + n);
    buf.set_position((pos + n) as u64);
    ret
}

fn read_u16(src: &mut Cursor<Bytes>) -> Result<u16, ParseError> {
    ensure!(src.remaining() >= 2, ParseError::InvalidLength);
    Ok(src.get_u16_be())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_variable_length() {
        macro_rules! assert_variable_length (
            ($bytes:expr, $res:expr) => {{
                assert_eq!(decode_variable_length($bytes), Ok(Some($res)));
            }};

            ($bytes:expr, $res:expr, $rest:expr) => {{
                assert_eq!(decode_variable_length($bytes), Ok(Some($res)));
            }};
        );

        assert_variable_length!(b"\x7f\x7f", 127, b"\x7f");

        assert_eq!(decode_variable_length(b"\xff\xff\xff"), Ok(None));
        assert_eq!(
            decode_variable_length(b"\xff\xff\xff\xff\xff\xff"),
            Err(ParseError::InvalidLength)
        );

        assert_variable_length!(b"\x00", 0);
        assert_variable_length!(b"\x7f", 127);
        assert_variable_length!(b"\x80\x01", 128);
        assert_variable_length!(b"\xff\x7f", 16383);
        assert_variable_length!(b"\x80\x80\x01", 16384);
        assert_variable_length!(b"\xff\xff\x7f", 2097151);
        assert_variable_length!(b"\x80\x80\x80\x01", 2097152);
        assert_variable_length!(b"\xff\xff\xff\x7f", 268435455);
    }

    // #[test]
    // fn test_decode_header() {
    //     assert_eq!(
    //         decode_header(b"\x20\x7f"),
    //         Done(
    //             &b""[..],
    //             FixedHeader {
    //                 packet_type: CONNACK,
    //                 packet_flags: 0,
    //                 remaining_length: 127,
    //             }
    //         )
    //     );

    //     assert_eq!(
    //         decode_header(b"\x3C\x82\x7f"),
    //         Done(
    //             &b""[..],
    //             FixedHeader {
    //                 packet_type: PUBLISH,
    //                 packet_flags: 0x0C,
    //                 remaining_length: 16258,
    //             }
    //         )
    //     );

    //     assert_eq!(decode_header(b"\x20"), Incomplete(Needed::Unknown));
    // }

    #[test]
    fn test_decode_connect_packets() {
        assert_eq!(
            decode_connect_packet(
                b"\x00\x04MQTT\x04\xC0\x00\x3C\x00\x0512345\x00\x04user\x00\x04pass"
            ),
            Ok(Packet::Connect {
                protocol: Protocol::MQTT(4),
                clean_session: false,
                keep_alive: 60,
                client_id: "12345".to_owned(),
                last_will: None,
                username: Some("user".to_owned()),
                password: Some(Bytes::from(&b"pass"[..])),
            })
        );

        assert_eq!(
            decode_connect_packet(
                b"\x00\x04MQTT\x04\x14\x00\x3C\x00\x0512345\x00\x05topic\x00\x07message"
            ),
            Ok(Packet::Connect {
                protocol: Protocol::MQTT(4),
                clean_session: false,
                keep_alive: 60,
                client_id: "12345".to_owned(),
                last_will: Some(LastWill {
                    qos: QoS::ExactlyOnce,
                    retain: false,
                    topic: "topic".to_owned(),
                    message: Bytes::from(&b"message"[..]),
                }),
                username: None,
                password: None,
            })
        );

        assert_eq!(
            decode_connect_packet(b"\x00\x02MQ"),
            Err(ParseError::InvalidProtocol),
        );
        assert_eq!(
            decode_connect_packet(b"\x00\x04MQAA"),
            Err(ParseError::InvalidProtocol),
        );
        assert_eq!(
            decode_connect_packet(b"\x00\x04MQTT\x03"),
            Err(ParseError::UnsupportedProtocolLevel),
        );
        assert_eq!(
            decode_connect_packet(b"\x00\x04MQTT\x04\xff"),
            Err(ParseError::ConnectReservedFlagSet)
        );

        assert_eq!(
            decode_connect_ack_packet(b"\x01\x04"),
            (SESSION_PRESENT, ConnectCode::BadUserNameOrPassword)
        );

        assert_eq!(
            decode_connect_ack_packet(b"\x03\x04"),
            Error(ErrorKind::Custom(RESERVED_FLAG))
        );

        assert_eq!(
            decode_packet(b"\x20\x02\x01\x04"),
            Done(
                &b""[..],
                Packet::ConnectAck {
                    session_present: true,
                    return_code: ConnectReturnCode::BadUserNameOrPassword,
                }
            )
        );

        assert_eq!(
            decode_packet(b"\xe0\x00"),
            Done(&b""[..], Packet::Disconnect)
        );
    }

    #[test]
    fn test_decode_publish_packets() {
        assert_eq!(
            decode_publish_header(b"\x00\x05topic\x12\x34"),
            Done(&b""[..], ("topic".to_owned(), 0x1234))
        );

        assert_eq!(
            decode_packet(b"\x3d\x0D\x00\x05topic\x43\x21data"),
            Done(
                &b""[..],
                Packet::Publish {
                    dup: true,
                    retain: true,
                    qos: QoS::ExactlyOnce,
                    topic: "topic".to_owned(),
                    packet_id: Some(0x4321),
                    payload: PayloadPromise::from(&b"data"[..]),
                }
            )
        );
        assert_eq!(
            decode_packet(b"\x30\x0b\x00\x05topicdata"),
            Done(
                &b""[..],
                Packet::Publish {
                    dup: false,
                    retain: false,
                    qos: QoS::AtMostOnce,
                    topic: "topic".to_owned(),
                    packet_id: None,
                    payload: PayloadPromise::from(&b"data"[..]),
                }
            )
        );

        assert_eq!(
            decode_packet(b"\x40\x02\x43\x21"),
            Done(&b""[..], Packet::PublishAck { packet_id: 0x4321 })
        );
        assert_eq!(
            decode_packet(b"\x50\x02\x43\x21"),
            Done(&b""[..], Packet::PublishReceived { packet_id: 0x4321 })
        );
        assert_eq!(
            decode_packet(b"\x60\x02\x43\x21"),
            Done(&b""[..], Packet::PublishRelease { packet_id: 0x4321 })
        );
        assert_eq!(
            decode_packet(b"\x70\x02\x43\x21"),
            Done(&b""[..], Packet::PublishComplete { packet_id: 0x4321 })
        );
    }

    #[test]
    fn test_decode_subscribe_packets() {
        let p = Packet::Subscribe {
            packet_id: 0x1234,
            topic_filters: vec![
                ("test".to_owned(), QoS::AtLeastOnce),
                ("filter".to_owned(), QoS::ExactlyOnce),
            ],
        };

        assert_eq!(
            decode_subscribe_header(b"\x12\x34\x00\x04test\x01\x00\x06filter\x02"),
            Done(&b""[..], p.clone())
        );
        assert_eq!(
            decode_packet(b"\x82\x12\x12\x34\x00\x04test\x01\x00\x06filter\x02"),
            Done(&b""[..], p)
        );

        let p = Packet::SubscribeAck {
            packet_id: 0x1234,
            status: vec![
                SubscribeReturnCode::Success(QoS::AtLeastOnce),
                SubscribeReturnCode::Failure,
                SubscribeReturnCode::Success(QoS::ExactlyOnce),
            ],
        };

        assert_eq!(
            decode_subscribe_ack_header(b"\x12\x34\x01\x80\x02"),
            Done(&b""[..], p.clone())
        );

        assert_eq!(
            decode_packet(b"\x90\x05\x12\x34\x01\x80\x02"),
            Done(&b""[..], p)
        );

        let p = Packet::Unsubscribe {
            packet_id: 0x1234,
            topic_filters: vec!["test".to_owned(), "filter".to_owned()],
        };

        assert_eq!(
            decode_unsubscribe_header(b"\x12\x34\x00\x04test\x00\x06filter"),
            Done(&b""[..], p.clone())
        );
        assert_eq!(
            decode_packet(b"\xa2\x10\x12\x34\x00\x04test\x00\x06filter"),
            Done(&b""[..], p)
        );

        assert_eq!(
            decode_packet(b"\xb0\x02\x43\x21"),
            Done(&b""[..], Packet::UnsubscribeAck { packet_id: 0x4321 })
        );
    }

    #[test]
    fn test_decode_ping_packets() {
        assert_eq!(
            decode_packet(b"\xc0\x00"),
            Done(&b""[..], Packet::PingRequest)
        );
        assert_eq!(
            decode_packet(b"\xd0\x00"),
            Done(&b""[..], Packet::PingResponse)
        );
    }
}
