use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};

use crate::buffer::ReadBuffer;
use crate::error::{Error, Result};

/// A peer-controlled TNS length must never turn into an unbounded allocation.
const MAX_TNS_PACKET_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Connect = 1,
    Accept = 2,
    Ack = 3,
    Refuse = 4,
    Redirect = 5,
    Data = 6,
    Null = 7,
    Abort = 9,
    Resend = 11,
    Marker = 12,
    Attention = 13,
    Control = 14,
}

impl TryFrom<u8> for PacketType {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(PacketType::Connect),
            2 => Ok(PacketType::Accept),
            3 => Ok(PacketType::Ack),
            4 => Ok(PacketType::Refuse),
            5 => Ok(PacketType::Redirect),
            6 => Ok(PacketType::Data),
            7 => Ok(PacketType::Null),
            9 => Ok(PacketType::Abort),
            11 => Ok(PacketType::Resend),
            12 => Ok(PacketType::Marker),
            13 => Ok(PacketType::Attention),
            14 => Ok(PacketType::Control),
            _ => Err(Error::InvalidPacketType(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SduMode {
    Small,
    Large,
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub packet_type: PacketType,
    pub flags: u8,
    pub payload: Bytes,
}

pub struct TnsStream {
    stream: TcpStream,
    mode: SduMode,
    idle_timeout: Option<Duration>,
    /// OCI thick clients frame their Fetch request with the full declared
    /// length and little-endian fixed fields, so the oracle-rs frame-completion
    /// fixup below must be skipped for them.
    oci_client: bool,
}

impl TnsStream {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            mode: SduMode::Small,
            idle_timeout: None,
            oci_client: false,
        }
    }

    pub fn set_mode(&mut self, mode: SduMode) {
        self.mode = mode;
    }

    /// Mark this connection as an OCI thick client (after protocol negotiation).
    pub fn set_oci_client(&mut self, oci: bool) {
        self.oci_client = oci;
    }

    /// Send a single TCP urgent (out-of-band, `MSG_OOB`) byte. Oracle uses this
    /// to signal an attention / break before an in-band Marker packet; the OCI
    /// client will not accept a mid-call error without it. Best-effort.
    pub async fn send_urgent_byte(&self, byte: u8) {
        let sock = socket2::SockRef::from(&self.stream);
        let _ = sock.send_out_of_band(&[byte]);
    }

    /// Limit how long a peer may leave a partially received TNS frame idle.
    /// A fresh deadline is applied to every socket read, so slow but active
    /// clients remain connected while abandoned sessions are eventually reaped.
    pub fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>) {
        self.idle_timeout = idle_timeout;
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        match self.idle_timeout {
            Some(idle_timeout) => match timeout(idle_timeout, self.stream.read_exact(buf)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => Err(error.into()),
                Err(_) => Err(Error::Protocol(format!(
                    "TNS peer was idle for {} seconds while a packet was incomplete",
                    idle_timeout.as_secs()
                ))),
            },
            None => {
                self.stream.read_exact(buf).await?;
                Ok(())
            }
        }
    }

    pub async fn read_packet(&mut self) -> Result<Packet> {
        let header = match self.mode {
            SduMode::Small => {
                let mut h = [0u8; 8];
                self.read_exact(&mut h).await?;
                h
            }
            SduMode::Large => {
                // Large SDU uses 4-byte length, then type, flags, header checksum (6 bytes total?)
                // Actually based on oracle-rs: for protocol >= 315, header is:
                // 4-byte length, 1-byte type, 1-byte flags, 2-byte checksum = 8 bytes
                let mut h = [0u8; 8];
                self.read_exact(&mut h).await?;
                h
            }
        };
        let (length, used_small_header) = packet_length(&header, self.mode);

        if !(8..=MAX_TNS_PACKET_SIZE).contains(&length) {
            return Err(Error::Protocol(format!(
                "invalid tns packet length {length}"
            )));
        }

        let packet_type = PacketType::try_from(header[4])?;
        let flags = header[5];

        let payload_len = length - 8;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            self.read_exact(&mut payload).await?;
        }
        // Real Oracle servers frame TTC DATA payloads by message content and treat
        // the packet length as advisory. oracle-rs 0.1.7's FetchMessage under-counts
        // the 2-byte data-flags field, leaving the trailing bytes of the request body
        // outside the declared length. Read the shortfall here so the wire stays in
        // sync; correctly-framed clients never trigger the extra read.
        if packet_type == PacketType::Data && payload.len() >= 5 && !self.oci_client {
            const TTC_MSG_FUNCTION: u8 = 0x03;
            const TTC_FUNC_FETCH: u8 = 0x05;
            if payload[2] == TTC_MSG_FUNCTION && payload[3] == TTC_FUNC_FETCH {
                let mut off = 5;
                off = self.complete_ub(&mut payload, off).await?; // cursor id (ub4)
                let _ = self.complete_ub(&mut payload, off).await?; // row count (ub4)
                tracing::debug!(
                    declared_length = length,
                    completed_length = payload.len(),
                    "completed Fetch TTC frame"
                );
            }
        }

        // DATA payloads include credentials and SQL values.  Never log their bytes.
        tracing::debug!(
            ?packet_type,
            length,
            used_small_header,
            payload_len = payload.len(),
            "TNS packet read"
        );
        if std::env::var("PGSACI_WIRE_DUMP").is_ok() {
            eprintln!(
                "<< {:?} len={} payload={}",
                packet_type,
                length,
                hex_dump(&payload)
            );
        }

        Ok(Packet {
            packet_type,
            flags,
            payload: Bytes::from(payload),
        })
    }

    async fn read_more(&mut self, buf: &mut Vec<u8>, n: usize) -> Result<()> {
        let start = buf.len();
        if start + n > MAX_TNS_PACKET_SIZE {
            return Err(Error::Protocol("oversized TNS packet completion".into()));
        }
        buf.resize(start + n, 0);
        self.read_exact(&mut buf[start..]).await?;
        Ok(())
    }

    /// Ensure `buf` holds a complete Oracle ub2/ub4/ub8 at `off`; return the offset
    /// just past it. Wire form: 1 length byte L (mask 0x7f, 0..=8) then L value bytes.
    async fn complete_ub(&mut self, buf: &mut Vec<u8>, off: usize) -> Result<usize> {
        if buf.len() <= off {
            self.read_more(buf, off + 1 - buf.len()).await?;
        }
        let l = (buf[off] & 0x7f) as usize;
        if l > 8 {
            return Err(Error::Protocol(format!("bad ub length {l}")));
        }
        let end = off + 1 + l;
        if buf.len() < end {
            self.read_more(buf, end - buf.len()).await?;
        }
        Ok(end)
    }

    pub async fn write_packet(&mut self, packet_type: PacketType, payload: &[u8]) -> Result<()> {
        self.write_packet_flags(packet_type, 0x00, payload).await
    }

    /// As [`write_packet`] but with an explicit TNS header flags byte (offset
    /// 5). MARKER packets from a real Oracle server carry `0x20` here; the OCI
    /// client only unwinds its in-flight call for a marker packet that has it
    /// set — a `0x00`-flagged marker is silently ignored and the client keeps
    /// re-driving the Execute.
    pub async fn write_packet_flags(
        &mut self,
        packet_type: PacketType,
        flags: u8,
        payload: &[u8],
    ) -> Result<()> {
        // OCI thick clients enforce the negotiated SDU: a single DATA packet
        // larger than it is rejected with `ORA-12592: TNS bad packet`. Split
        // an oversized TTC message the way a real Oracle server does — into
        // consecutive DATA packets each `<= 8111` bytes, every one carrying its
        // own 2-byte `00 00` data-flags header; the client's NS layer strips
        // those and concatenates the rest. (thin / oracle-rs reassemble a
        // single large packet fine, so only split for OCI.)
        const OCI_MAX_PACKET: usize = 8111;
        const OCI_CHUNK: usize = OCI_MAX_PACKET - 8 - 2; // 8101 message bytes
        if self.oci_client
            && packet_type == PacketType::Data
            && payload.len() > 2
            && 8 + payload.len() > OCI_MAX_PACKET
        {
            // Frame every SDU packet into ONE contiguous buffer and issue a
            // single `write_all` + `flush`. A large fetch reply can split into
            // hundreds of packets; flushing each one turned a 3 s stream into a
            // 25 s+ one under load, which tripped the corpus runner's watchdog
            // and wedged the shared connection.
            let data_flags = [payload[0], payload[1]];
            let body = &payload[2..];
            let npkts = body.len().div_ceil(OCI_CHUNK).max(1);
            let mut buf = Vec::with_capacity(payload.len() + npkts * 10);
            let mut off = 0;
            while off < body.len() {
                let end = (off + OCI_CHUNK).min(body.len());
                let plen = (2 + (end - off)) as u32;
                buf.extend_from_slice(&(8 + plen).to_be_bytes());
                buf.extend_from_slice(&[packet_type as u8, flags, 0x00, 0x00]);
                buf.extend_from_slice(&data_flags);
                buf.extend_from_slice(&body[off..end]);
                off = end;
            }
            self.stream.write_all(&buf).await?;
            self.stream.flush().await?;
            return Ok(());
        }
        self.emit_one_packet(packet_type, flags, payload).await
    }

    async fn emit_one_packet(
        &mut self,
        packet_type: PacketType,
        flags: u8,
        payload: &[u8],
    ) -> Result<()> {
        let packet_len = 8usize
            .checked_add(payload.len())
            .ok_or_else(|| Error::Protocol("TNS packet length overflow".to_string()))?;
        if packet_len > MAX_TNS_PACKET_SIZE {
            return Err(Error::Protocol(format!(
                "TNS packet exceeds maximum size: {packet_len}"
            )));
        }
        let mut buf = match self.mode {
            SduMode::Small => {
                if packet_len > u16::MAX as usize {
                    return Err(Error::Protocol(format!(
                        "small-SDU TNS packet too large: {packet_len}"
                    )));
                }
                let mut b = BytesMut::with_capacity(8 + payload.len());
                b.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
                b.extend_from_slice(&[0x00, 0x00]); // packet checksum
                b
            }
            SduMode::Large => {
                let mut b = BytesMut::with_capacity(8 + payload.len());
                b.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
                b
            }
        };

        buf.extend_from_slice(&[packet_type as u8, flags]); // type + flags
        buf.extend_from_slice(&[0x00, 0x00]); // header checksum
        buf.extend_from_slice(payload);

        tracing::debug!(packet_type = ?packet_type, length = buf.len(), "TNS packet written");
        if std::env::var("PGSACI_WIRE_DUMP").is_ok() {
            eprintln!(
                ">> {:?} len={} payload={}",
                packet_type,
                buf.len(),
                hex_dump(payload)
            );
        }
        self.stream.write_all(&buf).await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn write_marker(&mut self, marker_type: u8) -> Result<()> {
        self.write_packet(PacketType::Marker, &[marker_type, 0x00, 0x02])
            .await
    }

    /// A one-byte-data TNS marker as Oracle sends them to an OCI client:
    /// `[0x01][0x00][subtype]`, subtype 1 = BREAK, 2 = RESET.
    pub async fn write_oci_marker(&mut self, subtype: u8) -> Result<()> {
        self.write_packet_flags(PacketType::Marker, 0x20, &[0x01, 0x00, subtype])
            .await
    }

    /// Consume the RESET marker the client sends back during the BREAK/RESET
    /// exchange that precedes an OCI mid-call error. Reads a bounded number of
    /// marker packets, returning as soon as one is not a marker or the read
    /// times out. Never blocks the session for long.
    pub async fn drain_markers(&mut self, max: usize) -> Result<()> {
        for _ in 0..max {
            match timeout(Duration::from_millis(400), self.read_packet()).await {
                Ok(Ok(pkt)) if pkt.packet_type == PacketType::Marker => continue,
                _ => break,
            }
        }
        Ok(())
    }

    /// Read up to `max` marker packets from the client and reply to each with
    /// the `02 00 02` acknowledgement `race_break` uses for a client-initiated
    /// break. Used to drive the server-initiated cancel handshake (PG
    /// `statement_timeout`) through the exact packet sequence the OCI thick
    /// client accepts when it breaks on its own `call_timeout`.
    pub async fn ack_client_markers(&mut self, max: usize) -> Result<()> {
        for _ in 0..max {
            match timeout(Duration::from_millis(600), self.read_packet()).await {
                Ok(Ok(pkt)) if pkt.packet_type == PacketType::Marker => {
                    self.write_packet(PacketType::Marker, &[0x02, 0x00, 0x02])
                        .await?;
                }
                _ => break,
            }
        }
        Ok(())
    }
}

fn hex_dump(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(4096)];
    let mut s = String::with_capacity(shown.len() * 2);
    for b in shown {
        s.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > shown.len() {
        s.push_str(&format!("...(+{} bytes)", bytes.len() - shown.len()));
    }
    s
}

/// Determine the declared packet size without changing the negotiated SDU mode.
///
/// oracle-rs 0.1.7 emits Fetch packets with the legacy 2-byte header even
/// after negotiating large-SDU mode. Oracle servers accept that legacy frame,
/// so do likewise when its checksum slots are zero and its small length is a
/// valid complete TNS packet. Normal large-SDU traffic remains large-SDU.
fn packet_length(header: &[u8; 8], mode: SduMode) -> (usize, bool) {
    match mode {
        SduMode::Small => (u16::from_be_bytes([header[0], header[1]]) as usize, true),
        SduMode::Large => {
            let small_length = u16::from_be_bytes([header[0], header[1]]) as usize;
            let legacy_small_header =
                header[2] == 0 && header[3] == 0 && (8..=u16::MAX as usize).contains(&small_length);
            if legacy_small_header {
                (small_length, true)
            } else {
                (
                    u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize,
                    false,
                )
            }
        }
    }
}

pub fn build_accept_response(version: u16) -> Bytes {
    // Byte-for-byte the ACCEPT a real Oracle 21c server sends (37 data bytes),
    // with only the version substituted. Offsets that matter to the clients'
    // parsers, verified against a live capture + python-oracledb thin
    // `messages/connect.pyx`:
    //   0-1   protocol version           (must be <= what the client offered —
    //                                     OCI `nsaccept` rejects a higher one
    //                                     with ORA-12592)
    //   2-3   global service options     (0x0841)
    //   4-13  ignored (SDU16/TDU16/…); byte 12-13 carries the packet length
    //   14    NSI flags1 (0x41 = SUPPORT_SECURITY_RENEG | DISABLE_NA — the
    //         `NA_REQUIRED` bit stays clear so no Native Network Encryption)
    //   15-23 ignored
    //   24-27 SDU, 32-bit big-endian  = 8192  (python thin reads `caps.sdu` here)
    //   28-32 ignored (5 bytes)
    //   33-36 flags2, 32-bit  = 0  → FAST_AUTH and HAS_END_OF_RESPONSE both
    //         clear, so thin drivers keep classic framing
    // 61 data bytes: the first 37 match a real Oracle 21c ACCEPT (OCI's
    // `nsaccept` validates them and rejects anything else with ORA-12592); the
    // 24-byte all-zero trailer keeps ojdbc's `NIOAcceptPacket` (which reads a
    // `databaseUUID` at offset 37) and ODP.NET from underflowing. python-oracledb
    // thin stops at offset 37 and ignores the rest.
    let mut data = [0u8; 61];
    data[0..2].copy_from_slice(&version.to_be_bytes());
    // 0x0841, verbatim from a live Oracle 21c ACCEPT. The 0x0040 bit
    // (CAN_RECV_ATTENTION) makes python-oracledb thick arm OOB-urgent
    // monitoring, so a server-sent attention byte during a PG statement_timeout
    // triggers the client's own BREAK — the handshake it requires before it
    // will surface ORA-01013 instead of re-driving the Execute.
    data[2..4].copy_from_slice(&0x0841u16.to_be_bytes()); // global service options
    data[8..10].copy_from_slice(&0x0100u16.to_be_bytes()); // "value of 1 in hardware"
    data[12..14].copy_from_slice(&(8u16 + 61).to_be_bytes()); // total packet length
    data[14] = 0x41; // NSI flags
    data[15] = 0x41;
    data[24..28].copy_from_slice(&0x0000_2000u32.to_be_bytes()); // SDU 32-bit = 8192
    data[28..32].copy_from_slice(&0x0000_2000u32.to_be_bytes()); // TDU 32-bit = 8192 (matches live Oracle 21c)
    // data[33..37] = flags2 = 0; data[37..61] = trailer = 0 (already zeroed).
    Bytes::copy_from_slice(&data)
}

pub fn parse_connect_payload(payload: &[u8]) -> Result<(u16, u16, String)> {
    let mut buf = ReadBuffer::from_slice(payload);
    let desired_version = buf.read_u16_be()?;
    let minimum_version = buf.read_u16_be()?;

    // Skip to connect data offset and length
    buf.skip(14)?; // service options, SDU, TDU, proto chars, zeros, 1, connect data len
    let connect_data_offset = buf.read_u16_be()? as usize;

    if connect_data_offset <= payload.len() {
        let descriptor = std::str::from_utf8(&payload[connect_data_offset..])
            .map_err(|e| Error::DataConversionError(e.to_string()))?
            .to_string();
        Ok((desired_version, minimum_version, descriptor))
    } else {
        Ok((desired_version, minimum_version, String::new()))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{SduMode, TnsStream, packet_length};
    use crate::error::Error;

    #[test]
    fn accepts_legacy_small_header_while_in_large_sdu_mode() {
        // oracle-rs FetchMessage::new(1, 100): its header says 15 bytes, then
        // it writes the two omitted TTC data-flag bytes plus the Fetch body.
        let header = [0x00, 0x0f, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00];
        assert_eq!(packet_length(&header, SduMode::Large), (15, true));
    }

    #[test]
    fn retains_normal_large_sdu_header() {
        let header = [0x00, 0x00, 0x00, 0x18, 0x06, 0x00, 0x00, 0x00];
        assert_eq!(packet_length(&header, SduMode::Large), (24, false));
    }

    #[tokio::test]
    async fn reaps_an_incomplete_idle_packet() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let _connection = tokio::net::TcpStream::connect(address).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let (stream, _) = listener.accept().await.unwrap();
        let mut tns = TnsStream::new(stream);
        tns.set_idle_timeout(Some(Duration::from_millis(10)));

        let error = tns.read_packet().await.unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message.contains("idle")));
        client.await.unwrap();
    }
}
