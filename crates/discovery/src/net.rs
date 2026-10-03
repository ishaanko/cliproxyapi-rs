//! Multicast UDP sockets for mDNS (Go: libp2p/zeroconf connection.go). Unix only.
//!
//! Sockets bind the wildcard address on port 5353 with address reuse, join the mDNS group on every
//! selected interface and use `IP_PKTINFO` / `IPV6_PKTINFO` so each packet is received with, and
//! can be sent out through, a specific interface index.

use std::io::{self, IoSlice, IoSliceMut};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::socket::{
    ControlMessage, ControlMessageOwned, MsgFlags, SockaddrIn, SockaddrIn6, SockaddrStorage, recvmsg, sendmsg, setsockopt, sockopt,
};
use socket2::{Domain, InterfaceIndexOrAddress, Protocol, Socket, Type};

use crate::interfaces::Interface;

pub const MDNS_PORT: u16 = 5353;
pub const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);

/// How long a blocking read waits before the receive loop re-checks its stop flag.
const READ_TIMEOUT: Duration = Duration::from_millis(250);

/// One mDNS multicast socket (IPv4 or IPv6).
pub struct Conn {
    sock: UdpSocket,
    v6: bool,
}

/// A received datagram and where it came from.
pub struct Datagram {
    pub len: usize,
    pub from: SocketAddr,
    /// Index of the receiving interface, 0 when unknown.
    pub ifindex: u32,
}

fn names(interfaces: &[Interface]) -> String {
    let list: Vec<&str> = interfaces.iter().map(|i| i.name.as_str()).collect();
    format!("[{}]", list.join(" "))
}

impl Conn {
    /// Go `joinUdp4Multicast`.
    pub fn join_v4(interfaces: &[Interface]) -> Result<Conn, String> {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
        configure_reuse(&sock).map_err(|e| e.to_string())?;
        sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT).into()).map_err(|e| e.to_string())?;
        let _ = setsockopt(&sock, sockopt::Ipv4PacketInfo, &true);
        let mut failed = 0usize;
        for iface in interfaces {
            if sock.join_multicast_v4_n(&MDNS_GROUP_V4, &InterfaceIndexOrAddress::Index(iface.index)).is_err() {
                failed += 1;
            }
        }
        if failed == interfaces.len() {
            return Err(format!("udp4: failed to join any of these interfaces: {}", names(interfaces)));
        }
        let _ = sock.set_multicast_ttl_v4(255);
        Ok(Conn { sock: finish(sock)?, v6: false })
    }

    /// Go `joinUdp6Multicast`.
    pub fn join_v6(interfaces: &[Interface]) -> Result<Conn, String> {
        let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
        sock.set_only_v6(true).map_err(|e| e.to_string())?;
        configure_reuse(&sock).map_err(|e| e.to_string())?;
        sock.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, MDNS_PORT, 0, 0).into()).map_err(|e| e.to_string())?;
        let _ = setsockopt(&sock, sockopt::Ipv6RecvPacketInfo, &true);
        let mut failed = 0usize;
        for iface in interfaces {
            if sock.join_multicast_v6(&MDNS_GROUP_V6, iface.index).is_err() {
                failed += 1;
            }
        }
        if failed == interfaces.len() {
            return Err(format!("udp6: failed to join any of these interfaces: {}", names(interfaces)));
        }
        let _ = sock.set_multicast_hops_v6(255);
        Ok(Conn { sock: finish(sock)?, v6: true })
    }

    /// Waits up to the read timeout for a datagram; `Ok(None)` on timeout.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<Option<Datagram>> {
        let fd = self.sock.as_raw_fd();
        let mut iov = [IoSliceMut::new(buf)];
        let result = if self.v6 {
            let mut cmsg = nix::cmsg_space!(libc::in6_pktinfo);
            recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg), MsgFlags::empty()).map(|msg| {
                let ifindex = msg
                    .cmsgs()
                    .ok()
                    .and_then(|mut it| it.find_map(|c| if let ControlMessageOwned::Ipv6PacketInfo(info) = c { Some(info.ipi6_ifindex) } else { None }))
                    .unwrap_or(0);
                (msg.bytes, msg.address, ifindex)
            })
        } else {
            let mut cmsg = nix::cmsg_space!(libc::in_pktinfo);
            recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg), MsgFlags::empty()).map(|msg| {
                let ifindex = msg
                    .cmsgs()
                    .ok()
                    .and_then(|mut it| it.find_map(|c| if let ControlMessageOwned::Ipv4PacketInfo(info) = c { Some(info.ipi_ifindex as u32) } else { None }))
                    .unwrap_or(0);
                (msg.bytes, msg.address, ifindex)
            })
        };
        match result {
            Ok((len, address, ifindex)) => {
                let from = address.and_then(|a| {
                    if let Some(v4) = a.as_sockaddr_in() {
                        Some(SocketAddr::V4(SocketAddrV4::new(v4.ip(), v4.port())))
                    } else {
                        a.as_sockaddr_in6().map(|v6| SocketAddr::V6(SocketAddrV6::new(v6.ip(), v6.port(), v6.flowinfo(), v6.scope_id())))
                    }
                });
                match from {
                    Some(from) => Ok(Some(Datagram { len, from, ifindex })),
                    None => Ok(None),
                }
            }
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => Ok(None),
            Err(err) => Err(io::Error::from(err)),
        }
    }

    /// Sends `buf` to `dest`, through interface `ifindex` when it is non-zero.
    pub fn send_to(&self, buf: &[u8], dest: SocketAddr, ifindex: u32) -> io::Result<()> {
        let fd = self.sock.as_raw_fd();
        let iov = [IoSlice::new(buf)];
        let result = match dest {
            SocketAddr::V4(addr) => {
                let info = libc::in_pktinfo {
                    ipi_ifindex: ifindex as _,
                    ipi_spec_dst: libc::in_addr { s_addr: 0 },
                    ipi_addr: libc::in_addr { s_addr: 0 },
                };
                let cmsgs = [ControlMessage::Ipv4PacketInfo(&info)];
                let cmsgs: &[ControlMessage] = if ifindex != 0 { &cmsgs } else { &[] };
                sendmsg(fd, &iov, cmsgs, MsgFlags::empty(), Some(&SockaddrIn::from(addr)))
            }
            SocketAddr::V6(addr) => {
                let info = libc::in6_pktinfo { ipi6_addr: libc::in6_addr { s6_addr: [0; 16] }, ipi6_ifindex: ifindex as _ };
                let cmsgs = [ControlMessage::Ipv6PacketInfo(&info)];
                let cmsgs: &[ControlMessage] = if ifindex != 0 { &cmsgs } else { &[] };
                sendmsg(fd, &iov, cmsgs, MsgFlags::empty(), Some(&SockaddrIn6::from(addr)))
            }
        };
        result.map(|_| ()).map_err(io::Error::from)
    }
}

fn configure_reuse(sock: &Socket) -> io::Result<()> {
    sock.set_reuse_address(true)?;
    // Go sets SO_REUSEPORT as well on the BSDs so several listeners can share the port.
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    sock.set_reuse_port(true)?;
    Ok(())
}

fn finish(sock: Socket) -> Result<UdpSocket, String> {
    let sock: UdpSocket = sock.into();
    sock.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| e.to_string())?;
    Ok(sock)
}
