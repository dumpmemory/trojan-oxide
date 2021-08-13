use super::{error::eof_err, lite_tls_stream::Direction};
use crate::{expect_buf_len, utils::ParserError};
use anyhow::{Error, Result};
use std::{
    cmp::min,
    ops::{Deref, DerefMut},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "debug_info")]
use tracing::debug;

#[derive(Debug, Clone, Copy)]
pub(super) enum TlsVersion {
    Tls12,
    Tls13,
}

#[derive(Debug)]
pub struct TlsRelayBuffer {
    inner: Vec<u8>,
    /// read cursor
    cursor: usize,
}

impl Deref for TlsRelayBuffer {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for TlsRelayBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

fn extract_len(buf: &[u8]) -> usize {
    buf[0] as usize * 256 + buf[1] as usize
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Seen0x17 {
    None,
    FromInbound,
    FromOutbound,
    BothDirections,
}

impl Seen0x17 {
    fn witness(&mut self, dir: Direction) {
        use Direction::*;
        use Seen0x17::*;
        *self = match (*self, dir) {
            (None, Inbound) => FromInbound,
            (None, Outbound) => FromOutbound,
            (FromInbound, Outbound) | (FromOutbound, Inbound) => BothDirections,
            (BothDirections, _) => unreachable!(),
            _ => return,
        };
    }
}

impl TlsRelayBuffer {
    pub fn new() -> Self {
        Self {
            inner: Vec::with_capacity(2048),
            cursor: 0,
        }
    }
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn reset(&mut self) {
        unsafe {
            self.inner.set_len(0);
        }
        self.cursor = 0;
    }

    pub fn checked_packets(&self) -> &[u8] {
        if self.cursor < self.inner.len() {
            &self.inner[..self.cursor]
        } else {
            &self.inner
        }
    }

    pub fn pop_checked_packets(&mut self) {
        let new_len = self.inner.len() - min(self.cursor, self.inner.len());
        for i in 0..new_len {
            self.inner[i] = self.inner[self.cursor + i];
        }

        self.cursor -= self.inner.len() - new_len;

        unsafe {
            self.inner.set_len(new_len);
        }
    }

    pub fn check_client_hello(&mut self) -> Result<(), ParserError> {
        expect_buf_len!(self.inner, 5, "client hello incomplete[1]");
        if &self.inner[..3] != &[0x16, 0x03, 0x01] {
            // Not tls 1.2/1.3
            return Err(ParserError::Invalid("not tls 1.2/1.3[1]".into()));
        }
        self.cursor = 5 + extract_len(&self.inner[3..]);
        if self.cursor != self.inner.len() {
            // Not tls 1.2/1.3
            return Err(ParserError::Invalid("not tls 1.2/1.3[2]".into()));
        }
        Ok(())
    }

    pub(super) fn check_tls_packet(&mut self) -> Result<u8, ParserError> {
        expect_buf_len!(
            self.inner,
            self.cursor + 5,
            "packet 0x16 (or sth) incomplete[1]"
        );
        let packet_type = self.inner[self.cursor];
        self.cursor += 5 + extract_len(&self.inner[self.cursor + 3..]);
        expect_buf_len!(
            self.inner,
            self.cursor,
            "packet 0x16 (or sth) incomplete[2]"
        );
        Ok(packet_type)
    }

    pub(super) fn find_key_packets(
        &mut self,
        seen_0x17: &mut Seen0x17,
        dir: Direction,
    ) -> Result<TlsVersion, ParserError> {
        loop {
            expect_buf_len!(self.inner, self.cursor + 1, "find 0x17 incomplete");
            match self.inner[self.cursor] {
                0x17 => {
                    #[cfg(feature = "debug_info")]
                    debug!("found 0x17, already seen: {:?}", seen_0x17);
                    seen_0x17.witness(dir);
                    #[cfg(feature = "debug_info")]
                    debug!("now seen 0x17: {:?}", seen_0x17);
                    match seen_0x17 {
                        // Tls 1.2 full handshake and at client side
                        Seen0x17::FromInbound => return Ok(TlsVersion::Tls12),
                        _ => self.check_tls_packet()?,
                    };
                }
                0x14 => {
                    match seen_0x17 {
                        Seen0x17::FromOutbound => {
                            // we have a 0.5 rtt version for
                            // tls 1.3
                            return Ok(TlsVersion::Tls13);
                        }
                        _ => {
                            self.check_tls_packet()?;
                        }
                    }
                }
                0xff => {
                    // Tls 1.2 full handshake and at server side
                    return Ok(TlsVersion::Tls12);
                }
                0x15 | 0x16 => {
                    self.check_tls_packet()?;
                }
                _ => {
                    return Err(ParserError::Invalid("unexpected tls packet type".into()));
                }
            }
        }
    }

    pub(super) async fn flush_checked<W>(&mut self, writer: &mut W) -> Result<()>
    where
        W: AsyncWriteExt + Unpin,
    {
        if self.checked_packets().len() > 0 {
            if writer.write(self.checked_packets()).await? == 0 {
                return Err(eof_err("EOF on Parsing[]"));
            }
            writer.flush().await?;
            self.pop_checked_packets();
        }
        Ok(())
    }

    pub(super) async fn tls12_relay_helper<R, W>(
        &mut self,
        reader: &mut R,
        writer: &mut W,
    ) -> Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let mut key_packet_seen = 0;
        loop {
            while self.inner.len() < self.cursor + 5 {
                if self.checked_packets().len() > 0 {
                    self.flush_checked(writer).await?;
                }
                if reader.read_buf(self.deref_mut()).await? == 0 {
                    return Err(eof_err("EOF on Parsing[]"));
                }
            }

            // the tls 1.2 packets that we care:
            // 0x14 followed by 0x16 from target SERVER
            match self.inner[self.cursor] {
                0x14 => key_packet_seen = 1,
                0x16 if key_packet_seen == 1 => {
                    self.cursor += 5 + extract_len(&self.inner[self.cursor + 3..]);
                    while self.inner.len() < self.cursor {
                        if self.checked_packets().len() > 0 {
                            self.flush_checked(writer).await?;
                        }
                        if reader.read_buf(self.deref_mut()).await? == 0 {
                            return Err(eof_err("EOF on Parsing[]"));
                        }
                    }
                    self.flush_checked(writer).await?;
                    return Ok(());
                }
                _ => (),
            }
            self.cursor += 5 + extract_len(&self.inner[self.cursor + 3..]);
        }
    }

    pub(super) async fn read_single_small_packet<I>(&mut self, reader: &mut I) -> Result<u8>
    where
        I: AsyncReadExt + Unpin,
    {
        match self.check_tls_packet() {
            Ok(ty) => Ok(ty),
            Err(ParserError::Incomplete(_)) => {
                // We issue one more read. If it's again not ready,
                // it fails.
                if reader.read_buf(self.deref_mut()).await? == 0 {
                    return Err(eof_err("EOF on Parsing[13]"));
                }
                self.check_tls_packet().map_err(|e| Error::new(e))
            }
            Err(e @ ParserError::Invalid(_)) => Err(Error::new(e)),
        }
    }
}
