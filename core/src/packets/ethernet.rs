/*
* Copyright 2019 Comcast Cable Communications Management, LLC
*
* Licensed under the Apache License, Version 2.0 (the "License");
* you may not use this file except in compliance with the License.
* You may obtain a copy of the License at
*
* http://www.apache.org/licenses/LICENSE-2.0
*
* Unless required by applicable law or agreed to in writing, software
* distributed under the License is distributed on an "AS IS" BASIS,
* WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
* See the License for the specific language governing permissions and
* limitations under the License.
*
* SPDX-License-Identifier: Apache-2.0
*/

use crate::dpdk::BufferError;
use crate::net::MacAddr;
use crate::packets::{CondRc, Header, Packet};
use crate::{ensure, Mbuf, Result, SizeOf};
use std::fmt;
use std::ptr::NonNull;

// Tag protocol identifiers.
const VLAN_802_1Q: u16 = 0x8100;
const VLAN_802_1AD: u16 = 0x88a8;

/// Ethernet II frame.
///
/// This is an implementation of the Ethernet II frame specified in IEEE
/// 802.3. The payload can have a size up to the MTU of 1500 octets, or
/// more in the case of jumbo frames. The frame check sequence or FCS that
/// follows the payload is handled by the hardware and is not included.
///
/// ```
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |  Dst MAC  |  Src MAC  |Typ|             Payload               |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+                                   |
/// |                                                               |
/// |                                                               |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// Destination MAC     48-bit MAC address of the originator of the
///                     packet.
///
/// Source MAC          48-bit MAC address of the intended recipient of
///                     the packet.
///
/// Ether Type          16-bit indicator. Identifies which protocol is
///                     encapsulated in the payload of the frame.
///
/// # 802.1Q
///
/// For networks support virtual LANs, the frame may include an extra VLAN
/// tag after the source MAC as specified in IEEE 802.1Q.
///
/// ```
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |  Dst MAC  |  Src MAC  | V-TAG |Typ|          Payload          |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// The tag has the following format, with TPID set to `0x8100`.
///
/// ```
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |   16 bits   | 3 bits  | 1 bit | 12 bits |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |             |            TCI            |
/// +    TPID     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |             |   PCP   |  DEI  |   VID   |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// TPID                16-bit tag protocol identifier, located at the same
///                     position as the EtherType field in untagged frames.
///
/// TCI                 16-bit tag control information containing the following
///                     sub-fields.
///
/// PCP                 3-bit priority code point which refers to the IEEE
///                     802.1p class of service and maps to the frame priority
///                     level.
///
/// DEI                 1-bit drop eligible indicator, may be used separately
///                     or in conjunction with PCP to indicate frames eligible
///                     to be dropped in the presence of congestion.
///
/// VID                 12-bit VLAN identifier specifying the VLAN to which the
///                     frame belongs.
///
/// # 802.1ad
///
/// The frame may be double tagged as per IEEE 802.1ad.
///
/// ```
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |  Dst MAC  |  Src MAC  | S-TAG | C-TAG |Typ|     Payload       |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// Double tagging can be useful for ISPs, allowing them to use VLANs internally
/// while mixing traffic from clients that are already VLAN tagged. The outer
/// S-TAG, or service tag, comes first, followed by the inner C-TAG, or customer
/// tag. In such cases, 802.1ad specifies a TPID of `0x88a8` for S-TAG.
#[derive(Clone)]
pub struct Ethernet {
    envelope: CondRc<Mbuf>,
    header: NonNull<EthernetHeader>,
    offset: usize,
}

impl Ethernet {
    /// Returns the source MAC address.
    #[inline]
    pub fn src(&self) -> MacAddr {
        self.header().src
    }

    /// Sets the source MAC address.
    #[inline]
    pub fn set_src(&mut self, src: MacAddr) {
        self.header_mut().src = src
    }

    /// Returns the destination MAC address.
    #[inline]
    pub fn dst(&self) -> MacAddr {
        self.header().dst
    }

    /// Sets the destination MAC address.
    #[inline]
    pub fn set_dst(&mut self, dst: MacAddr) {
        self.header_mut().dst = dst
    }

    /// Returns the marker that indicates whether the frame is VLAN.
    #[inline]
    fn vlan_marker(&self) -> u16 {
        unsafe { u16::from_be(self.header().chunk.ether_type) }
    }

    /// Returns the protocol identifier of the payload.
    #[inline]
    pub fn ether_type(&self) -> EtherType {
        let header = self.header();
        let ether_type = unsafe {
            match self.vlan_marker() {
                VLAN_802_1Q => header.chunk.chunk_802_1q.ether_type,
                VLAN_802_1AD => header.chunk.chunk_802_1ad.ether_type,
                _ => header.chunk.ether_type,
            }
        };

        EtherType::new(u16::from_be(ether_type))
    }

    /// Sets the protocol identifier of the payload.
    #[inline]
    pub fn set_ether_type(&mut self, ether_type: EtherType) {
        let ether_type = u16::to_be(ether_type.0);
        match self.vlan_marker() {
            VLAN_802_1Q => self.header_mut().chunk.chunk_802_1q.ether_type = ether_type,
            VLAN_802_1AD => self.header_mut().chunk.chunk_802_1ad.ether_type = ether_type,
            _ => self.header_mut().chunk.ether_type = ether_type,
        }
    }

    /// Returns whether the frame is VLAN 802.1Q tagged.
    #[inline]
    pub fn is_vlan_802_1q(&self) -> bool {
        self.vlan_marker() == VLAN_802_1Q
    }

    /// Returns whether the frame is VLAN 802.1ad tagged.
    #[inline]
    pub fn is_vlan_802_1ad(&self) -> bool {
        self.vlan_marker() == VLAN_802_1AD
    }

    /// Swaps the source MAC address with the destination MAC address.
    #[inline]
    pub fn swap_addresses(&mut self) {
        let src = self.src();
        let dst = self.dst();
        self.set_src(dst);
        self.set_dst(src);
    }
}

impl fmt::Debug for Ethernet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ethernet")
            .field("src", &format!("{}", self.src()))
            .field("dst", &format!("{}", self.dst()))
            .field("ether_type", &format!("{}", self.ether_type()))
            .field("vlan", &(self.is_vlan_802_1q() || self.is_vlan_802_1ad()))
            .field("$offset", &self.offset())
            .field("$len", &self.len())
            .field("$header_len", &self.header_len())
            .finish()
    }
}

impl Packet for Ethernet {
    type Header = EthernetHeader;
    type Envelope = Mbuf;

    #[inline]
    fn envelope(&self) -> &Self::Envelope {
        &self.envelope
    }

    #[inline]
    fn envelope_mut(&mut self) -> &mut Self::Envelope {
        &mut self.envelope
    }

    #[doc(hidden)]
    #[inline]
    fn header(&self) -> &Self::Header {
        unsafe { self.header.as_ref() }
    }

    #[doc(hidden)]
    #[inline]
    fn header_mut(&mut self) -> &mut Self::Header {
        unsafe { self.header.as_mut() }
    }

    #[inline]
    fn offset(&self) -> usize {
        self.offset
    }

    #[inline]
    fn header_len(&self) -> usize {
        if self.is_vlan_802_1q() {
            Self::Header::size_of() + VlanTag::size_of()
        } else if self.is_vlan_802_1ad() {
            Self::Header::size_of() + VlanTag::size_of() * 2
        } else {
            Self::Header::size_of()
        }
    }

    #[doc(hidden)]
    #[inline]
    fn do_parse(envelope: Self::Envelope) -> Result<Self> {
        let mbuf = envelope.mbuf();
        let offset = envelope.payload_offset();
        let header = mbuf.read_data(offset)?;

        let packet = Ethernet {
            envelope: CondRc::new(envelope),
            header,
            offset,
        };

        // we've only parsed 14 bytes as the ethernet header, in case of
        // vlan, we need to make sure there's enough data for the whole
        // header including tags, otherwise accessing the union type in the
        // header will cause a panic.
        ensure!(
            packet.mbuf().data_len() >= packet.header_len(),
            BufferError::OutOfBuffer(packet.header_len(), packet.mbuf().data_len())
        );

        Ok(packet)
    }

    #[doc(hidden)]
    #[inline]
    fn do_push(mut envelope: Self::Envelope) -> Result<Self> {
        let offset = envelope.payload_offset();
        let mbuf = envelope.mbuf_mut();

        mbuf.extend(offset, Self::Header::size_of())?;
        let header = mbuf.write_data(offset, &Self::Header::default())?;

        Ok(Ethernet {
            envelope: CondRc::new(envelope),
            header,
            offset,
        })
    }

    #[inline]
    fn remove(mut self) -> Result<Self::Envelope> {
        let offset = self.offset();
        let len = self.header_len();
        self.mbuf_mut().shrink(offset, len)?;
        Ok(self.envelope.into_owned())
    }

    #[inline]
    fn deparse(self) -> Self::Envelope {
        self.envelope.into_owned()
    }
}

/// The protocol identifier of the ethernet frame payload.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[repr(C, packed)]
pub struct EtherType(pub u16);

impl EtherType {
    pub fn new(value: u16) -> Self {
        EtherType(value)
    }
}

/// Supported ethernet payload protocol types.
#[allow(non_snake_case)]
#[allow(non_upper_case_globals)]
pub mod EtherTypes {
    use super::EtherType;

    // Address resolution protocol.
    pub const Arp: EtherType = EtherType(0x0806);
    // Internet Protocol version 4.
    pub const Ipv4: EtherType = EtherType(0x0800);
    // Internet Protocol version 6.
    pub const Ipv6: EtherType = EtherType(0x86DD);
}

impl fmt::Display for EtherType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                EtherTypes::Arp => "ARP".to_string(),
                EtherTypes::Ipv4 => "IPv4".to_string(),
                EtherTypes::Ipv6 => "IPv6".to_string(),
                _ => {
                    let t = self.0;
                    format!("0x{:04x}", t)
                }
            }
        )
    }
}

/// VLAN tag.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
pub struct VlanTag {
    tpid: u16,
    tci: u16,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
impl VlanTag {
    /// Returns the tag protocol identifier, either 802.1q or 802.1ad.
    pub fn tag_id(&self) -> u16 {
        self.tpid
    }

    /// Returns the priority code point.
    pub fn priority(&self) -> u8 {
        (self.tci >> 13) as u8
    }

    /// Returns whether the frame is eligible to be dropped in the presence
    /// of congestion.
    pub fn drop_eligible(&self) -> bool {
        self.tci & 0x1000 > 0
    }

    /// Returns the VLAN identifier.
    pub fn identifier(&self) -> u16 {
        self.tci & 0x0fff
    }
}

#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
pub struct Chunk802_1q {
    tag: VlanTag,
    ether_type: u16,
}

#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
pub struct Chunk802_1ad {
    stag: VlanTag,
    ctag: VlanTag,
    ether_type: u16,
}

/// The ethernet header chunk follows the source mac addr.
#[derive(Clone, Copy)]
#[repr(C, packed)]
pub union Chunk {
    ether_type: u16,
    chunk_802_1q: Chunk802_1q,
    chunk_802_1ad: Chunk802_1ad,
}

impl Default for Chunk {
    fn default() -> Chunk {
        Chunk { ether_type: 0 }
    }
}

/// Ethernet header.
#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
pub struct EthernetHeader {
    dst: MacAddr,
    src: MacAddr,
    chunk: Chunk,
}

impl Header for EthernetHeader {}

impl SizeOf for EthernetHeader {
    /// Size of the ethernet header.
    ///
    /// Because the ethernet header is not fixed and modeled with a union, the
    /// memory layout size is not the correct header size. For a brand new
    /// ethernet header, we will always report 14 bytes as the fixed portion,
    /// which is the minimum size without any tags. `Ethernet::header_len()`
    /// will report the correct instance size based on the presence or absence
    /// of VLAN tags.
    #[inline]
    fn size_of() -> usize {
        14
    }
}

#[cfg(any(test, feature = "testils"))]
#[rustfmt::skip]
pub const VLAN_802_1Q_PACKET: [u8; 64] = [
// ethernet header
    0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
    // tpid
    0x81, 0x00,
    // tci
    0x00, 0x7b,
    // ether type
    0x08, 0x06,
    // payload
    0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x02, 0x00, 0x19,
    0x06, 0xea, 0xb8, 0xc1, 0xc0, 0xa8, 0x7b, 0x01, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xc0, 0xa8, 0x7b, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00
];

#[cfg(any(test, feature = "testils"))]
#[rustfmt::skip]
pub const VLAN_802_1AD_PACKET: [u8; 68] = [
// ethernet header
    0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
    // tpid
    0x88, 0xa8,
    // tci
    0x00, 0x1e,
    // tpid
    0x81, 0x00,
    // tci
    0x20, 0x65,
    // ether type
    0x08, 0x06,
    // payload
    0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x02, 0x00, 0x19,
    0x06, 0xea, 0xb8, 0xc1, 0xc0, 0xa8, 0x7b, 0x01, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xc0, 0xa8, 0x7b, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packets::UDP_PACKET;

    #[test]
    fn size_of_ethernet_header() {
        assert_eq!(14, EthernetHeader::size_of());
    }

    #[test]
    fn ether_type_to_string() {
        assert_eq!("ARP", EtherTypes::Arp.to_string());
        assert_eq!("IPv4", EtherTypes::Ipv4.to_string());
        assert_eq!("IPv6", EtherTypes::Ipv6.to_string());
        assert_eq!("0x0000", EtherType::new(0).to_string());
    }

    #[capsule::test]
    fn parse_ethernet_packet() {
        let packet = Mbuf::from_bytes(&UDP_PACKET).unwrap();
        let ethernet = packet.parse::<Ethernet>().unwrap();

        assert_eq!("00:00:00:00:00:01", ethernet.dst().to_string());
        assert_eq!("00:00:00:00:00:02", ethernet.src().to_string());
        assert_eq!(EtherTypes::Ipv4, ethernet.ether_type());
    }

    #[capsule::test]
    fn parse_vlan_802_1q_packet() {
        let packet = Mbuf::from_bytes(&VLAN_802_1Q_PACKET).unwrap();
        let ethernet = packet.parse::<Ethernet>().unwrap();

        assert_eq!("00:00:00:00:00:01", ethernet.dst().to_string());
        assert_eq!("00:00:00:00:00:02", ethernet.src().to_string());
        assert!(ethernet.is_vlan_802_1q());
        assert_eq!(EtherTypes::Arp, ethernet.ether_type());
        assert_eq!(18, ethernet.header_len());
    }

    #[capsule::test]
    fn parse_vlan_802_1ad_packet() {
        let packet = Mbuf::from_bytes(&VLAN_802_1AD_PACKET).unwrap();
        let ethernet = packet.parse::<Ethernet>().unwrap();

        assert_eq!("00:00:00:00:00:01", ethernet.dst().to_string());
        assert_eq!("00:00:00:00:00:02", ethernet.src().to_string());
        assert!(ethernet.is_vlan_802_1ad());
        assert_eq!(EtherTypes::Arp, ethernet.ether_type());
        assert_eq!(22, ethernet.header_len());
    }

    #[capsule::test]
    fn swap_addresses() {
        let packet = Mbuf::from_bytes(&UDP_PACKET).unwrap();
        let mut ethernet = packet.parse::<Ethernet>().unwrap();
        ethernet.swap_addresses();

        assert_eq!("00:00:00:00:00:02", ethernet.dst().to_string());
        assert_eq!("00:00:00:00:00:01", ethernet.src().to_string());
    }

    #[capsule::test]
    fn push_ethernet_packet() {
        let packet = Mbuf::new().unwrap();
        let ethernet = packet.push::<Ethernet>().unwrap();

        assert_eq!(EthernetHeader::size_of(), ethernet.len());
    }
}
