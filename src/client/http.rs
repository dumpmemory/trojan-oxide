use anyhow::{Error, Result};
use futures::future;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::*;
use tokio::net::TcpStream;
use tracing::*;
use crate::utils::ParserError;

pub struct Target {
    is_https: bool,
    host: String,
    port: u16,
    cursor: usize,
}

const HEADER0: &'static [u8] = b"GET / HTTP/1.1\r\nHost: ";
const HEADER1: &'static [u8] = b"\r\nConnection: keep-alive\r\n\r\n";

impl Target {
    pub fn new() -> Self {
        Self {
            is_https: false,
            host: String::new(),
            port: 0,
            cursor: 0,
        }
    }

    fn set_stream_type(&mut self, buf: &Vec<u8>) -> Result<(), ParserError> {
        if buf.len() < 4 {
            return Err(ParserError::Incomplete);
        }

        if &buf[..4] == b"GET " {
            self.is_https = false;
            self.cursor = 4;
            return Ok(());
        }

        if buf.len() < 8 {
            return Err(ParserError::Incomplete);
        }

        if &buf[..8] == b"CONNECT " {
            self.is_https = true;
            self.cursor = 8;
            return Ok(());
        }

        return Err(ParserError::Invalid);
    }

    fn set_host(&mut self, buf: &Vec<u8>) -> Result<(), ParserError> {
        while self.cursor < buf.len() && buf[self.cursor] == b' ' {
            self.cursor += 1;
        }
        if !self.is_https {
            if self.cursor + 7 < buf.len() {
                if &buf[self.cursor..self.cursor + 7].to_ascii_lowercase()[..] == b"http://" {
                    self.cursor += 7;
                }
            } else {
                return Err(ParserError::Incomplete);
            }
        }

        let start = self.cursor;
        let mut end = start;
        while end < buf.len() && buf[end] != b' ' && buf[end] != b'/' {
            end += 1;
        }

        if end == buf.len() {
            return Err(ParserError::Incomplete);
        }

        let mut port_idx = end;
        for i in (start..end).rev() {
            if buf[i] == b':' {
                port_idx = i;
                break;
            }
        }

        if port_idx + 1 == end {
            return Err(ParserError::Invalid);
        } else if port_idx == end {
            if !self.is_https {
                self.port = 80;
            } else {
                return Err(ParserError::Invalid);
            }
        } else {
            for i in port_idx..end {
                let di = buf[i];
                if di >= b'0' && di <= b'9' {
                    self.port = self.port * 10 + (di - b'0') as u16;
                } else {
                    return Err(ParserError::Invalid);
                }
            }
        }

        self.host =
            String::from_utf8(buf[start..port_idx].to_vec()).map_err(|_| ParserError::Invalid)?;

        return Ok(());
    }

    fn parse(&mut self, buf: &mut Vec<u8>) -> Result<(), ParserError> {
        trace!("parsing: {:?}", String::from_utf8(buf.clone()));
        if self.cursor == 0 {
            self.set_stream_type(buf)?;
        }

        trace!("stream is https: {}", self.is_https);

        if self.host.len() == 0 {
            self.set_host(buf)?;
        }

        trace!("stream target host: {}", self.host);

        // `integrity` check
        if &buf[buf.len() - 4..] == b"\r\n\r\n" {
            trace!("integrity test passed");
            return Ok(());
        }

        for i in 0..4 {
            buf[i] = buf[buf.len() - 4 + i];
        }

        unsafe {
            buf.set_len(4);
        }
        Err(ParserError::Incomplete)
    }

    pub async fn accept(&mut self, inbound: &mut TcpStream) -> Result<()> {
        let mut buffer = Vec::with_capacity(200);
        loop {
            let read = inbound.read_buf(&mut buffer).await?;
            if read != 0 {
                match self.parse(&mut buffer) {
                    Ok(_) => {
                        trace!("stream parsed");
                        break;
                    }
                    Err(ParserError::Invalid) => {
                        return Err(Error::new(ParserError::Invalid));
                    }
                    _ => (),
                }
            } else {
                return Err(Error::new(ParserError::Invalid));
            }
        }

        if self.is_https {
            inbound
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
            trace!("https packet 0 sent");
        }

        Ok(())
    }

    pub async fn send_packet0<A>(&self, outbound: &mut A, password_hash: Arc<String>) -> Result<()>
    where
        A: AsyncWrite + Unpin + ?Sized,
    {
        let command0 = [b'\r', b'\n', 1, 0];
        let port_arr = self.port.to_be_bytes();
        let packet0 = [
            IoSlice::new(password_hash.as_bytes()),
            IoSlice::new(&command0),
            IoSlice::new(self.host.as_bytes()),
            IoSlice::new(&port_arr),
            IoSlice::new(&[b'\r', b'\n']),
        ];
        let mut writer = Pin::new(outbound);
        future::poll_fn(|cx| writer.as_mut().poll_write_vectored(cx, &packet0[..]))
            .await
            .map_err(|e| Box::new(e))?;

        if !self.is_https {
            let bufs = [
                IoSlice::new(HEADER0),
                IoSlice::new(self.host.as_bytes()),
                IoSlice::new(HEADER1),
            ];

            future::poll_fn(|cx| writer.as_mut().poll_write_vectored(cx, &bufs[..]))
                .await
                .map_err(|e| Box::new(e))?;

            trace!("http packet 0 sent");
        }
        trace!("trojan packet 0 sent");
        writer.flush().await.map_err(|e| Box::new(e))?;

        Ok(())
    }
}
