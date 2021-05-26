use futures::ready;

use super::{
    client_udp_stream::{UdpRead, UdpWrite},
    CursoredBuffer, MixAddrType, UdpRelayBuffer,
};
use std::pin::Pin;
use std::task::Poll;
use std::{future::Future, u64};
use tracing::debug;

pub async fn copy_udp<'a, R: UdpRead + Unpin, W: UdpWrite + Unpin>(
    reader: &'a mut R,
    writer: &'a mut W,
) -> std::io::Result<u64> {
    CopyUdp {
        reader,
        writer,
        buf: UdpRelayBuffer::new(),
        addr: None,
        amt: 0,
    }
    .await
}

struct CopyUdp<'a, R: UdpRead, W: UdpWrite> {
    reader: &'a mut R,
    writer: &'a mut W,
    buf: UdpRelayBuffer,
    addr: Option<MixAddrType>,
    amt: u64,
}

impl<R, W> Future for CopyUdp<'_, R, W>
where
    R: UdpRead + Unpin,
    W: UdpWrite + Unpin,
{
    type Output = std::io::Result<u64>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let me = &mut *self;
        loop {
            if me.addr.is_none() {
                let new_addr =
                    ready!(Pin::new(&mut *me.reader).poll_proxy_stream_read(cx, &mut me.buf))?;
                if new_addr.is_none() {
                    return Poll::Ready(Ok(me.amt));
                }
                me.addr = Some(new_addr);
            }
            let x = ready!(Pin::new(&mut *me.writer).poll_proxy_stream_write(
                cx,
                &me.buf.as_bytes(),
                me.addr.as_ref().unwrap()
            ))?;
            me.buf.advance(x);

            if !me.buf.has_remaining() {
                debug!("udp packet not finished in one write");
                me.addr = None;
                unsafe {
                    me.buf.reset();
                }
            }
            me.amt += x as u64;
        }
    }
}
