macro_rules! env_or_default {
    ($key:literal) => {
        match option_env!($key) {
            Some(val) => val,
            None => "",
        }
    };
}

pub const IP: &str = env_or_default!("AX_IP");
pub const GATEWAY: &str = env_or_default!("AX_GW");
pub const IP_PREFIX: u8 = 24;

pub const STANDARD_MTU: usize = 1500;
pub const LOOPBACK_MTU: usize = 64 * 1024 - 40;
pub const LOOPBACK_TCP_MSS: usize = LOOPBACK_MTU - 40;

// Socket buffers. Larger buffers raise loopback throughput (iperf/netperf) and
// cut packet loss (the previous 64 KiB + 64-packet router/loopback queues led
// to 54% UDP loss under iperf). SO_SNDBUF/SO_RCVBUF from userspace is currently
// a no-op for capacity, so these constants are the effective sizes.
pub const TCP_RX_BUF_LEN: usize = 256 * 1024;
pub const TCP_TX_BUF_LEN: usize = 256 * 1024;
pub const UDP_RX_BUF_LEN: usize = 4 * 1024 * 1024;
pub const UDP_TX_BUF_LEN: usize = 4 * 1024 * 1024;
pub const LISTEN_QUEUE_SIZE: usize = 512;

// Packet-count for the router/loopback queues (each entry holds up to one MTU).
// Raised from 256 to 512 so bursty loopback traffic doesn't overflow and drop.
pub const SOCKET_BUFFER_SIZE: usize = 1024;
pub const ETHERNET_MAX_PENDING_PACKETS: usize = 128;
