use crate::utils::{
    CursoredBuffer, ExtendableFromSlice, MixAddrType, UdpRead, UdpRelayBuffer, UdpWrite,
};
use futures::ready;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{
    net::SocketAddr,
    ops::{Deref, DerefMut},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tracing::{debug, warn};

#[derive(Debug)]
struct Socks5UdpSpecifiedBuffer {
    inner: Vec<u8>,
}

impl Socks5UdpSpecifiedBuffer {
    fn new(capacity: usize) -> Self {
        let mut inner = Vec::with_capacity(capacity);
        // The fields in the UDP request header are:
        //     o  RSV  Reserved X'0000'
        //     o  FRAG    Current fragment number
        inner.extend_from_slice(&[0, 0, 0]);
        Self { inner }
    }

    fn reset(&mut self) {
        unsafe {
            self.inner.set_len(3);
        }
    }

    fn is_empty(&self) -> bool {
        assert!(
            self.inner.len() >= 3,
            "Socks5UdpSpecifiedBuffer unexpected len: {}",
            self.inner.len()
        );
        self.inner.len() == 3
    }
}

impl ExtendableFromSlice for Socks5UdpSpecifiedBuffer {
    fn extend_from_slice(&mut self, src: &[u8]) {
        self.inner.extend_from_slice(src);
    }
}

impl Deref for Socks5UdpSpecifiedBuffer {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Socks5UdpSpecifiedBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub struct Socks5UdpStream {
    server_udp_socket: UdpSocket,
    client_udp_addr: Option<SocketAddr>,
    signal_reset: oneshot::Receiver<()>,
}

#[derive(Debug)]
pub struct Socks5UdpRecvStream<'a> {
    server_udp_socket: &'a UdpSocket,
    client_udp_addr: Option<SocketAddr>,
    addr_tx: Option<oneshot::Sender<SocketAddr>>,
    signal_reset: &'a mut oneshot::Receiver<()>,
}

impl<'a> Socks5UdpRecvStream<'a> {
    fn new(
        server_udp_socket: &'a UdpSocket,
        addr_tx: oneshot::Sender<SocketAddr>,
        signal_reset: &'a mut oneshot::Receiver<()>,
    ) -> Self {
        Self {
            server_udp_socket,
            client_udp_addr: None,
            addr_tx: Some(addr_tx),
            signal_reset,
        }
    }
}

#[derive(Debug)]
pub struct Socks5UdpSendStream<'a> {
    server_udp_socket: &'a UdpSocket,
    client_udp_addr: Option<SocketAddr>,
    addr_rx: Option<oneshot::Receiver<SocketAddr>>,
    buffer: Socks5UdpSpecifiedBuffer,
}

impl<'a> Socks5UdpSendStream<'a> {
    fn new(server_udp_socket: &'a UdpSocket, addr_tx: oneshot::Receiver<SocketAddr>) -> Self {
        Self {
            server_udp_socket,
            client_udp_addr: None,
            addr_rx: Some(addr_tx),
            buffer: Socks5UdpSpecifiedBuffer::new(2048),
        }
    }
}

impl Socks5UdpStream {
    pub fn new(
        server_udp_socket: UdpSocket,
        stream_reset_signal_rx: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            server_udp_socket,
            client_udp_addr: None,
            signal_reset: stream_reset_signal_rx,
        }
    }

    pub fn split<'a>(&'a mut self) -> (Socks5UdpSendStream<'a>, Socks5UdpRecvStream<'a>) {
        let (addr_tx, addr_rx) = oneshot::channel();
        (
            Socks5UdpSendStream::new(&self.server_udp_socket, addr_rx),
            Socks5UdpRecvStream::new(&self.server_udp_socket, addr_tx, &mut self.signal_reset),
        )
    }
}

impl<'a> AsyncRead for Socks5UdpRecvStream<'a> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let addr = match ready!(self.server_udp_socket.poll_recv_from(cx, buf)) {
            Ok(addr) => addr,
            Err(e) => {
                return Poll::Ready(Err(e));
            }
        };

        if self.client_udp_addr.is_none() {
            self.client_udp_addr = Some(addr.clone());
            let addr_tx = match self.addr_tx.take() {
                Some(v) => v,
                None => {
                    return Poll::Ready(Err(std::io::ErrorKind::Other.into()));
                }
            };
            match addr_tx.send(addr) {
                Ok(_) => {
                    return Poll::Ready(Ok(()));
                }
                Err(_) => {
                    return Poll::Ready(Err(std::io::ErrorKind::Other.into()));
                }
            }
        } else {
            if self.client_udp_addr.unwrap() != addr {
                return Poll::Ready(Err(std::io::ErrorKind::Interrupted.into()));
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<'a> Socks5UdpSendStream<'a> {
    fn poll_write_optioned(
        self: &mut std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: Option<&[u8]>,
    ) -> Poll<Result<usize, std::io::Error>> {
        if self.client_udp_addr.is_none() {
            let maybe_addr = match self.addr_rx {
                Some(ref mut rx) => rx.try_recv(),
                None => {
                    return Poll::Ready(Err(std::io::ErrorKind::Other.into()));
                }
            };

            self.client_udp_addr = match maybe_addr {
                Ok(addr) => Some(addr),
                Err(_) => {
                    return Poll::Ready(Err(std::io::ErrorKind::WouldBlock.into()));
                }
            }
        }

        let buf = match buf {
            Some(b) => b,
            None => &self.buffer,
        };

        self.server_udp_socket
            .poll_send_to(cx, buf, self.client_udp_addr.unwrap())
    }
}

impl<'a> AsyncWrite for Socks5UdpSendStream<'a> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        (&mut self).poll_write_optioned(cx, Some(buf))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl<'a> UdpRead for Socks5UdpRecvStream<'a> {
    /// ```not_rust
    /// +----+------+------+----------+----------+----------+
    /// |RSV | FRAG | ATYP | DST.ADDR | DST.PORT |   DATA   |
    /// +----+------+------+----------+----------+----------+
    /// | 2  |  1   |  1   | Variable |    2     | Variable |
    /// +----+------+------+----------+----------+----------+
    /// The fields in the UDP request header are:
    ///      o  RSV  Reserved X'0000'
    ///      o  FRAG    Current fragment number
    ///      o  ATYP    address type of following addresses:
    ///          o  IP V4 address: X'01'
    ///          o  DOMAINNAME: X'03'
    ///          o  IP V6 address: X'04'
    ///      o  DST.ADDR       desired destination address
    ///      o  DST.PORT       desired destination port
    ///      o  DATA     user data
    /// ```
    fn poll_proxy_stream_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut UdpRelayBuffer,
    ) -> Poll<std::io::Result<MixAddrType>> {
        debug!("Socks5UdpRecvStream::poll_proxy_stream_read()");
        let mut buf_inner = buf.as_read_buf();
        let ptr = buf_inner.filled().as_ptr();

        crate::try_recv!(
            oneshot,
            self.signal_reset,
            return Poll::Ready(Ok(MixAddrType::None))
        );

        match ready!(self.poll_read(cx, &mut buf_inner)) {
            Ok(_) => {
                // Ensure the pointer does not change from under us
                assert_eq!(ptr, buf_inner.filled().as_ptr());
                let n = buf_inner.filled().len();

                if n < 3 {
                    return Poll::Ready(Ok(MixAddrType::None));
                }

                // Safety: This is guaranteed to be the number of initialized (and read)
                // bytes due to the invariants provided by `ReadBuf::filled`.
                unsafe {
                    buf.advance_mut(n);
                }
                buf.advance(3);
                debug!(
                    "Socks5UdpRecvStream::poll_proxy_stream_read() buf {:?}",
                    buf
                );
                Poll::Ready(
                    MixAddrType::from_encoded(buf).map_err(|_| std::io::ErrorKind::Other.into()),
                )
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl<'a> UdpWrite for Socks5UdpSendStream<'a> {
    fn poll_proxy_stream_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        addr: &MixAddrType,
    ) -> Poll<std::io::Result<usize>> {
        let just_filled_buf = if self.buffer.is_empty() {
            addr.write_buf(&mut self.buffer);
            self.buffer.extend_from_slice(buf);
            true
        } else {
            false
        };

        // only if we write the whole buf in one write we reset the buffer
        // to accept new data.
        match self.poll_write_optioned(cx, None)? {
            Poll::Ready(real_written_amt) => {
                if real_written_amt == self.buffer.len() {
                    self.buffer.reset();
                } else {
                    warn!("Socks5UdpSendStream didn't send the entire buffer");
                }
            }
            _ => (),
        }

        if just_filled_buf {
            Poll::Ready(Ok(buf.len()))
        } else {
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(self, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        todo!()
    }
}