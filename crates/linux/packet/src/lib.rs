//! Pure Linux AF_PACKET policy values and first-stage socket state.
//!
//! This crate owns normalized protocol and address values, bind publication,
//! ordinary receive-view decisions, supported packet options, and typed
//! conversion of endpoint-owned destructive statistics snapshots. It
//! deliberately does not own packet buffers, live counters, device taps,
//! queues, waiters, file descriptors, userspace memory, capabilities, network
//! namespaces, or syscall error conversion.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod address;
mod error;
mod options;
mod protocol;
mod receive;
mod socket;
mod socket_filter;
mod statistics;

pub use address::{
    AF_PACKET, InterfaceIndex, LinkLayerAddress, LinkLayerInfo, MAX_LINK_LAYER_ADDRESS_LEN,
    PacketBindRequest, PacketSendAddress, PacketType, SockAddrLl,
};
pub use error::PacketError;
pub use options::{
    GetPacketOption, PacketOption, PacketOptionOperation, PacketOptionValue, SetPacketOption,
};
pub use protocol::{ETH_P_ALL, EtherType, ProtocolSelector};
pub use receive::{
    FrameLayout, MSG_PEEK, MSG_TRUNC, PacketView, QueueDisposition, ReceiveDecision, ReceiveFlags,
};
pub use socket::{
    BindPlan, BindPublication, BindingGeneration, DeliveryDecision, DeliveryDirection,
    PacketBinding, PacketSocketState, PacketSocketType,
};
pub use socket_filter::{
    SKF_AD_ALU_XOR_X, SKF_AD_CPU, SKF_AD_HATYPE, SKF_AD_IFINDEX, SKF_AD_MARK, SKF_AD_NLATTR,
    SKF_AD_NLATTR_NEST, SKF_AD_OFF, SKF_AD_PAY_OFFSET, SKF_AD_PKTTYPE, SKF_AD_PROTOCOL,
    SKF_AD_QUEUE, SKF_AD_RANDOM, SKF_AD_RXHASH, SKF_AD_VLAN_TAG, SKF_AD_VLAN_TAG_PRESENT,
    SKF_AD_VLAN_TPID, SocketFilterAncillary, SocketFilterSnapshot,
    classify_socket_filter_ancillary, encoded_socket_filter_ancillary,
    is_socket_filter_ancillary_offset, socket_filter_ancillary_value,
};
pub use statistics::PacketStatistics;
