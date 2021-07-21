use std::time::Duration;

pub const HASH_LEN: usize = 56;
#[allow(dead_code)]
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
pub const ECHO_PHRASE: &str = "echo";
pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
pub const MAX_CONCURRENT_BIDI_STREAMS: usize = 30;

pub enum ServiceMode {
    TCP,
    UDP,
    #[cfg(feature = "mini_tls")]
    MiniTLS,
}

impl ServiceMode {
    pub fn get_code(&self) -> u8 {
        match self {
            ServiceMode::TCP => 0x01,
            ServiceMode::UDP => 0x03,
            #[cfg(feature = "mini_tls")]
            ServiceMode::MiniTLS => 0x11,
        }
    }
}
