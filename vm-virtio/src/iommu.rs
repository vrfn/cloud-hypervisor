// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use epoll;
use libc::EFD_NONBLOCK;
use std::cmp;
use std::collections::BTreeMap;
use std::fmt::{self, Display};
use std::io::{self, Write};
use std::mem::size_of;
use std::ops::Bound::Included;
use std::os::unix::io::AsRawFd;
use std::result;
use std::sync::{Arc, RwLock};
use std::thread;

use super::Error as DeviceError;
use super::{
    ActivateError, ActivateResult, DescriptorChain, DeviceEventT, Queue, VirtioDevice,
    VirtioDeviceType, VIRTIO_F_VERSION_1,
};
use crate::{DmaRemapping, VirtioInterrupt, VirtioInterruptType};
use vm_device::ExternalDmaMapping;
use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryError, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

/// Queues sizes
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE; NUM_QUEUES];

/// New descriptors are pending on the request queue.
/// "requestq" is meant to be used anytime an action is required to be
/// performed on behalf of the guest driver.
const REQUEST_Q_EVENT: DeviceEventT = 0;
/// New descriptors are pending on the event queue.
/// "eventq" lets the device report any fault or other asynchronous event to
/// the guest driver.
const EVENT_Q_EVENT: DeviceEventT = 1;
/// The device has been dropped.
const KILL_EVENT: DeviceEventT = 2;

/// Virtio IOMMU features
#[allow(unused)]
const VIRTIO_IOMMU_F_INPUT_RANGE: u32 = 0;
#[allow(unused)]
const VIRTIO_IOMMU_F_DOMAIN_BITS: u32 = 1;
#[allow(unused)]
const VIRTIO_IOMMU_F_MAP_UNMAP: u32 = 2;
#[allow(unused)]
const VIRTIO_IOMMU_F_BYPASS: u32 = 3;
#[allow(unused)]
const VIRTIO_IOMMU_F_PROBE: u32 = 4;

// Support 2MiB and 4KiB page sizes.
#[allow(unused)]
const VIRTIO_IOMMU_PAGE_SIZE_MASK: u64 = (2 << 20) | (4 << 10);

#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuRange {
    start: u64,
    end: u64,
}

unsafe impl ByteValued for VirtioIommuRange {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuConfig {
    page_size_mask: u64,
    input_range: VirtioIommuRange,
    domain_bits: u8,
    padding: [u8; 3],
    probe_size: u32,
}

unsafe impl ByteValued for VirtioIommuConfig {}

/// Virtio IOMMU request type
const VIRTIO_IOMMU_T_ATTACH: u8 = 1;
const VIRTIO_IOMMU_T_DETACH: u8 = 2;
const VIRTIO_IOMMU_T_MAP: u8 = 3;
const VIRTIO_IOMMU_T_UNMAP: u8 = 4;
const VIRTIO_IOMMU_T_PROBE: u8 = 5;

#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuReqHead {
    type_: u8,
    reserved: [u8; 3],
}

unsafe impl ByteValued for VirtioIommuReqHead {}

/// Virtio IOMMU request status
const VIRTIO_IOMMU_S_OK: u8 = 0;
#[allow(unused)]
const VIRTIO_IOMMU_S_IOERR: u8 = 1;
#[allow(unused)]
const VIRTIO_IOMMU_S_UNSUPP: u8 = 2;
#[allow(unused)]
const VIRTIO_IOMMU_S_DEVERR: u8 = 3;
#[allow(unused)]
const VIRTIO_IOMMU_S_INVAL: u8 = 4;
#[allow(unused)]
const VIRTIO_IOMMU_S_RANGE: u8 = 5;
#[allow(unused)]
const VIRTIO_IOMMU_S_NOENT: u8 = 6;
#[allow(unused)]
const VIRTIO_IOMMU_S_FAULT: u8 = 7;

#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuReqTail {
    status: u8,
    reserved: [u8; 3],
}

unsafe impl ByteValued for VirtioIommuReqTail {}

/// ATTACH request
#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuReqAttach {
    domain: u32,
    endpoint: u32,
    reserved: [u8; 8],
}

unsafe impl ByteValued for VirtioIommuReqAttach {}

/// DETACH request
#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuReqDetach {
    domain: u32,
    endpoint: u32,
    reserved: [u8; 8],
}

unsafe impl ByteValued for VirtioIommuReqDetach {}

/// Virtio IOMMU request MAP flags
#[allow(unused)]
const VIRTIO_IOMMU_MAP_F_READ: u32 = 1;
#[allow(unused)]
const VIRTIO_IOMMU_MAP_F_WRITE: u32 = 1 << 1;
#[allow(unused)]
const VIRTIO_IOMMU_MAP_F_EXEC: u32 = 1 << 2;
#[allow(unused)]
const VIRTIO_IOMMU_MAP_F_MMIO: u32 = 1 << 3;

/// MAP request
#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuReqMap {
    domain: u32,
    virt_start: u64,
    virt_end: u64,
    phys_start: u64,
    flags: u32,
}

unsafe impl ByteValued for VirtioIommuReqMap {}

/// UNMAP request
#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuReqUnmap {
    domain: u32,
    virt_start: u64,
    virt_end: u64,
    reserved: [u8; 4],
}

unsafe impl ByteValued for VirtioIommuReqUnmap {}

/// Virtio IOMMU request PROBE types
#[allow(unused)]
const VIRTIO_IOMMU_PROBE_T_MASK: u32 = 0xfff;
#[allow(unused)]
const VIRTIO_IOMMU_PROBE_T_NONE: u32 = 0;
#[allow(unused)]
const VIRTIO_IOMMU_PROBE_T_RESV_MEM: u32 = 1;

/// PROBE request
#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuReqProbe {
    endpoint: u32,
    reserved: [u64; 8],
}

unsafe impl ByteValued for VirtioIommuReqProbe {}

#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuProbeProperty {
    type_: u16,
    length: u16,
}

unsafe impl ByteValued for VirtioIommuProbeProperty {}

/// Virtio IOMMU request PROBE property RESV_MEM subtypes
#[allow(unused)]
const VIRTIO_IOMMU_RESV_MEM_T_RESERVED: u32 = 0;
#[allow(unused)]
const VIRTIO_IOMMU_RESV_MEM_T_MSI: u32 = 1;

#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuProbeResvMem {
    head: VirtioIommuProbeProperty,
    subtype: u8,
    reserved: [u8; 3],
    start: u64,
    end: u64,
}

unsafe impl ByteValued for VirtioIommuProbeResvMem {}

/// Virtio IOMMU fault flags
#[allow(unused)]
const VIRTIO_IOMMU_FAULT_F_READ: u32 = 1;
#[allow(unused)]
const VIRTIO_IOMMU_FAULT_F_WRITE: u32 = 1 << 1;
#[allow(unused)]
const VIRTIO_IOMMU_FAULT_F_EXEC: u32 = 1 << 2;
#[allow(unused)]
const VIRTIO_IOMMU_FAULT_F_ADDRESS: u32 = 1 << 8;

/// Virtio IOMMU fault reasons
#[allow(unused)]
const VIRTIO_IOMMU_FAULT_R_UNKNOWN: u32 = 0;
#[allow(unused)]
const VIRTIO_IOMMU_FAULT_R_DOMAIN: u32 = 1;
#[allow(unused)]
const VIRTIO_IOMMU_FAULT_R_MAPPING: u32 = 2;

/// Fault reporting through eventq
#[allow(unused)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(packed)]
struct VirtioIommuFault {
    reason: u8,
    reserved: [u8; 3],
    flags: u32,
    endpoint: u32,
    reserved1: u32,
    address: u64,
}

unsafe impl ByteValued for VirtioIommuFault {}

#[derive(Debug)]
enum Error {
    /// Guest gave us bad memory addresses.
    GuestMemory(GuestMemoryError),
    /// Guest gave us a write only descriptor that protocol says to read from.
    UnexpectedWriteOnlyDescriptor,
    /// Guest gave us a read only descriptor that protocol says to write to.
    UnexpectedReadOnlyDescriptor,
    /// Guest gave us too few descriptors in a descriptor chain.
    DescriptorChainTooShort,
    /// Guest gave us a buffer that was too short to use.
    BufferLengthTooSmall,
    /// Guest sent us invalid request.
    InvalidRequest,
    /// Guest sent us invalid ATTACH request.
    InvalidAttachRequest,
    /// Guest sent us invalid DETACH request.
    InvalidDetachRequest,
    /// Guest sent us invalid MAP request.
    InvalidMapRequest,
    /// Guest sent us invalid UNMAP request.
    InvalidUnmapRequest,
    /// Guest sent us invalid PROBE request.
    InvalidProbeRequest,
    /// Failed to performing external mapping.
    ExternalMapping(io::Error),
    /// Failed to performing external unmapping.
    ExternalUnmapping(io::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            BufferLengthTooSmall => write!(f, "buffer length too small"),
            DescriptorChainTooShort => write!(f, "descriptor chain too short"),
            GuestMemory(e) => write!(f, "bad guest memory address: {}", e),
            InvalidRequest => write!(f, "invalid request"),
            InvalidAttachRequest => write!(f, "invalid attach request"),
            InvalidDetachRequest => write!(f, "invalid detach request"),
            InvalidMapRequest => write!(f, "invalid map request"),
            InvalidUnmapRequest => write!(f, "invalid unmap request"),
            InvalidProbeRequest => write!(f, "invalid probe request"),
            UnexpectedReadOnlyDescriptor => write!(f, "unexpected read-only descriptor"),
            UnexpectedWriteOnlyDescriptor => write!(f, "unexpected write-only descriptor"),
            ExternalMapping(e) => write!(f, "failed performing external mapping: {}", e),
            ExternalUnmapping(e) => write!(f, "failed performing external unmapping: {}", e),
        }
    }
}

#[derive(Debug, PartialEq)]
enum RequestType {
    Attach,
    Detach,
    Map,
    Unmap,
    Probe,
}

struct Request {
    #[allow(unused)]
    type_: RequestType,
    status_addr: GuestAddress,
}

impl Request {
    // Parse the available vring buffer. Based on the hashmap table of external
    // mappings required from various devices such as VFIO or vhost-user ones,
    // this function might update the hashmap table of external mappings per
    // domain.
    // Basically, the VMM knows about the device_id <=> mapping relationship
    // before running the VM, but at runtime, a new domain <=> mapping hashmap
    // is created based on the information provided from the guest driver for
    // virtio-iommu (giving the link device_id <=> domain).
    fn parse(
        avail_desc: &DescriptorChain,
        mem: &GuestMemoryMmap,
        mapping: &Arc<IommuMapping>,
        ext_mapping: &BTreeMap<u32, Arc<dyn ExternalDmaMapping>>,
        ext_domain_mapping: &mut BTreeMap<u32, Arc<dyn ExternalDmaMapping>>,
    ) -> result::Result<Request, Error> {
        // The head contains the request type which MUST be readable.
        if avail_desc.is_write_only() {
            return Err(Error::UnexpectedWriteOnlyDescriptor);
        }

        if (avail_desc.len as usize) < size_of::<VirtioIommuReqHead>() {
            return Err(Error::InvalidRequest);
        }

        let req_head: VirtioIommuReqHead =
            mem.read_obj(avail_desc.addr).map_err(Error::GuestMemory)?;
        let req_offset = size_of::<VirtioIommuReqHead>();
        let desc_size_left = (avail_desc.len as usize) - req_offset;
        let req_addr = if let Some(addr) = avail_desc.addr.checked_add(req_offset as u64) {
            addr
        } else {
            return Err(Error::InvalidRequest);
        };

        let request_type = match req_head.type_ {
            VIRTIO_IOMMU_T_ATTACH => {
                if desc_size_left != size_of::<VirtioIommuReqAttach>() {
                    return Err(Error::InvalidAttachRequest);
                }

                let req: VirtioIommuReqAttach = mem
                    .read_obj(req_addr as GuestAddress)
                    .map_err(Error::GuestMemory)?;
                debug!("Attach request {:?}", req);

                // Copy the value to use it as a proper reference.
                let domain = req.domain;
                let endpoint = req.endpoint;

                // Add endpoint associated with specific domain
                mapping.endpoints.write().unwrap().insert(endpoint, domain);

                // If the endpoint is part of the list of devices with an
                // external mapping, insert a new entry for the corresponding
                // domain, with the same reference to the trait.
                if let Some(map) = ext_mapping.get(&endpoint) {
                    ext_domain_mapping.insert(domain, map.clone());
                }

                // Add new domain with no mapping if the entry didn't exist yet
                let mut mappings = mapping.mappings.write().unwrap();
                if !mappings.contains_key(&domain) {
                    mappings.insert(domain, BTreeMap::new());
                }

                RequestType::Attach
            }
            VIRTIO_IOMMU_T_DETACH => {
                if desc_size_left != size_of::<VirtioIommuReqDetach>() {
                    return Err(Error::InvalidDetachRequest);
                }

                let req: VirtioIommuReqDetach = mem
                    .read_obj(req_addr as GuestAddress)
                    .map_err(Error::GuestMemory)?;
                debug!("Detach request {:?}", req);

                // Copy the value to use it as a proper reference.
                let domain = req.domain;
                let endpoint = req.endpoint;

                // If the endpoint is part of the list of devices with an
                // external mapping, remove the entry for the corresponding
                // domain.
                if ext_mapping.contains_key(&endpoint) {
                    ext_domain_mapping.remove(&domain);
                }

                // Remove endpoint associated with specific domain
                mapping.endpoints.write().unwrap().remove(&endpoint);

                RequestType::Detach
            }
            VIRTIO_IOMMU_T_MAP => {
                if desc_size_left != size_of::<VirtioIommuReqMap>() {
                    return Err(Error::InvalidMapRequest);
                }

                let req: VirtioIommuReqMap = mem
                    .read_obj(req_addr as GuestAddress)
                    .map_err(Error::GuestMemory)?;
                debug!("Map request {:?}", req);

                // Copy the value to use it as a proper reference.
                let domain = req.domain;

                // Trigger external mapping if necessary.
                if let Some(ext_map) = ext_domain_mapping.get(&domain) {
                    let size = req.virt_end - req.virt_start + 1;
                    ext_map
                        .map(req.virt_start, req.phys_start, size)
                        .map_err(Error::ExternalMapping)?;
                }

                // Add new mapping associated with the domain
                if let Some(entry) = mapping.mappings.write().unwrap().get_mut(&domain) {
                    entry.insert(
                        req.virt_start,
                        Mapping {
                            gpa: req.phys_start,
                            size: req.virt_end - req.virt_start + 1,
                        },
                    );
                } else {
                    return Err(Error::InvalidMapRequest);
                }

                RequestType::Map
            }
            VIRTIO_IOMMU_T_UNMAP => {
                if desc_size_left != size_of::<VirtioIommuReqUnmap>() {
                    return Err(Error::InvalidUnmapRequest);
                }

                let req: VirtioIommuReqUnmap = mem
                    .read_obj(req_addr as GuestAddress)
                    .map_err(Error::GuestMemory)?;
                debug!("Unmap request {:?}", req);

                // Copy the value to use it as a proper reference.
                let domain = req.domain;
                let virt_start = req.virt_start;

                // Trigger external unmapping if necessary.
                if let Some(ext_map) = ext_domain_mapping.get(&domain) {
                    let size = req.virt_end - virt_start + 1;
                    ext_map
                        .unmap(virt_start, size)
                        .map_err(Error::ExternalUnmapping)?;
                }

                // Add new mapping associated with the domain
                if let Some(entry) = mapping.mappings.write().unwrap().get_mut(&domain) {
                    entry.remove(&virt_start);
                }

                RequestType::Unmap
            }
            VIRTIO_IOMMU_T_PROBE => {
                if desc_size_left != size_of::<VirtioIommuReqProbe>() {
                    return Err(Error::InvalidProbeRequest);
                }

                let req: VirtioIommuReqProbe = mem
                    .read_obj(req_addr as GuestAddress)
                    .map_err(Error::GuestMemory)?;
                debug!("Probe request {:?}", req);

                RequestType::Probe
            }
            _ => return Err(Error::InvalidRequest),
        };

        let status_desc = avail_desc
            .next_descriptor()
            .ok_or(Error::DescriptorChainTooShort)?;

        // The status MUST always be writable
        if !status_desc.is_write_only() {
            return Err(Error::UnexpectedReadOnlyDescriptor);
        }

        if (status_desc.len as usize) < size_of::<VirtioIommuReqTail>() {
            return Err(Error::BufferLengthTooSmall);
        }

        Ok(Request {
            type_: request_type,
            status_addr: status_desc.addr,
        })
    }
}

struct IommuEpollHandler {
    queues: Vec<Queue>,
    mem: Arc<RwLock<GuestMemoryMmap>>,
    interrupt_cb: Arc<VirtioInterrupt>,
    queue_evts: Vec<EventFd>,
    kill_evt: EventFd,
    mapping: Arc<IommuMapping>,
    ext_mapping: BTreeMap<u32, Arc<dyn ExternalDmaMapping>>,
    ext_domain_mapping: BTreeMap<u32, Arc<dyn ExternalDmaMapping>>,
}

impl IommuEpollHandler {
    fn request_queue(&mut self) -> bool {
        let mut used_desc_heads = [(0, 0); QUEUE_SIZE as usize];
        let mut used_count = 0;
        let mem = self.mem.read().unwrap();
        for avail_desc in self.queues[0].iter(&mem) {
            let len = match Request::parse(
                &avail_desc,
                &mem,
                &self.mapping,
                &self.ext_mapping,
                &mut self.ext_domain_mapping,
            ) {
                Ok(ref req) => {
                    let reply = VirtioIommuReqTail {
                        status: VIRTIO_IOMMU_S_OK,
                        ..Default::default()
                    };

                    match mem.write_obj(reply, req.status_addr) {
                        Ok(_) => size_of::<VirtioIommuReqTail>() as u32,
                        Err(e) => {
                            error!("bad guest memory address: {}", e);
                            0
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to parse available descriptor chain: {:?}", e);
                    0
                }
            };

            used_desc_heads[used_count] = (avail_desc.index, len);
            used_count += 1;
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            self.queues[0].add_used(&mem, desc_index, len);
        }
        used_count > 0
    }

    fn event_queue(&mut self) -> bool {
        false
    }

    fn signal_used_queue(&self, queue: &Queue) -> result::Result<(), DeviceError> {
        (self.interrupt_cb)(&VirtioInterruptType::Queue, Some(queue)).map_err(|e| {
            error!("Failed to signal used queue: {:?}", e);
            DeviceError::FailedSignalingUsedQueue(e)
        })
    }

    fn run(&mut self) -> result::Result<(), DeviceError> {
        // Create the epoll file descriptor
        let epoll_fd = epoll::create(true).map_err(DeviceError::EpollCreateFd)?;

        // Add events
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.queue_evts[0].as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(REQUEST_Q_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.queue_evts[1].as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(EVENT_Q_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.kill_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(KILL_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;

        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];

        'epoll: loop {
            let num_events = match epoll::wait(epoll_fd, -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // It's well defined from the epoll_wait() syscall
                        // documentation that the epoll loop can be interrupted
                        // before any of the requested events occurred or the
                        // timeout expired. In both those cases, epoll_wait()
                        // returns an error of type EINTR, but this should not
                        // be considered as a regular error. Instead it is more
                        // appropriate to retry, by calling into epoll_wait().
                        continue;
                    }
                    return Err(DeviceError::EpollWait(e));
                }
            };

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    REQUEST_Q_EVENT => {
                        if let Err(e) = self.queue_evts[0].read() {
                            error!("Failed to get queue event: {:?}", e);
                            break 'epoll;
                        } else if self.request_queue() {
                            if let Err(e) = self.signal_used_queue(&self.queues[0]) {
                                error!("Failed to signal used queue: {:?}", e);
                                break 'epoll;
                            }
                        }
                    }
                    EVENT_Q_EVENT => {
                        if let Err(e) = self.queue_evts[1].read() {
                            error!("Failed to get queue event: {:?}", e);
                            break 'epoll;
                        } else if self.event_queue() {
                            if let Err(e) = self.signal_used_queue(&self.queues[1]) {
                                error!("Failed to signal used queue: {:?}", e);
                                break 'epoll;
                            }
                        }
                    }
                    KILL_EVENT => {
                        debug!("kill_evt received, stopping epoll loop");
                        break 'epoll;
                    }
                    _ => {
                        error!("Unknown event for virtio-iommu");
                        break 'epoll;
                    }
                }
            }

            info!("Exit epoll loop");
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Mapping {
    gpa: u64,
    size: u64,
}

pub struct IommuMapping {
    // Domain related to an endpoint.
    endpoints: Arc<RwLock<BTreeMap<u32, u32>>>,
    // List of mappings per domain.
    mappings: Arc<RwLock<BTreeMap<u32, BTreeMap<u64, Mapping>>>>,
}

impl DmaRemapping for IommuMapping {
    fn translate(&self, id: u32, addr: u64) -> std::result::Result<u64, std::io::Error> {
        debug!("Translate addr 0x{:x}", addr);
        if let Some(domain) = self.endpoints.read().unwrap().get(&id) {
            if let Some(mapping) = self.mappings.read().unwrap().get(domain) {
                let range_start = if VIRTIO_IOMMU_PAGE_SIZE_MASK > addr {
                    0
                } else {
                    addr - VIRTIO_IOMMU_PAGE_SIZE_MASK
                };
                for (&key, &value) in mapping.range((Included(&range_start), Included(&addr))) {
                    if addr >= key && addr < key + value.size {
                        let new_addr = addr - key + value.gpa;
                        debug!("Into new_addr 0x{:x}", new_addr);
                        return Ok(new_addr);
                    }
                }
            }
        }

        debug!("Into same addr...");
        Ok(addr)
    }
}

pub struct Iommu {
    kill_evt: Option<EventFd>,
    avail_features: u64,
    acked_features: u64,
    config: VirtioIommuConfig,
    mapping: Arc<IommuMapping>,
    ext_mapping: BTreeMap<u32, Arc<dyn ExternalDmaMapping>>,
    queue_evts: Option<Vec<EventFd>>,
    interrupt_cb: Option<Arc<VirtioInterrupt>>,
}

impl Iommu {
    pub fn new() -> io::Result<(Self, Arc<IommuMapping>)> {
        let config = VirtioIommuConfig {
            page_size_mask: VIRTIO_IOMMU_PAGE_SIZE_MASK,
            ..Default::default()
        };

        let mapping = Arc::new(IommuMapping {
            endpoints: Arc::new(RwLock::new(BTreeMap::new())),
            mappings: Arc::new(RwLock::new(BTreeMap::new())),
        });

        Ok((
            Iommu {
                kill_evt: None,
                avail_features: 1u64 << VIRTIO_F_VERSION_1 | 1u64 << VIRTIO_IOMMU_F_MAP_UNMAP,
                acked_features: 0u64,
                config,
                mapping: mapping.clone(),
                ext_mapping: BTreeMap::new(),
                queue_evts: None,
                interrupt_cb: None,
            },
            mapping,
        ))
    }

    pub fn add_external_mapping(&mut self, device_id: u32, mapping: Arc<dyn ExternalDmaMapping>) {
        self.ext_mapping.insert(device_id, mapping);
    }
}

impl Drop for Iommu {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }
    }
}

impl VirtioDevice for Iommu {
    fn device_type(&self) -> u32 {
        VirtioDeviceType::TYPE_IOMMU as u32
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            // Get the lower 32-bits of the features bitfield.
            0 => self.avail_features as u32,
            // Get the upper 32-bits of the features bitfield.
            1 => (self.avail_features >> 32) as u32,
            _ => {
                warn!("Received request for unknown features page.");
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => {
                warn!("Cannot acknowledge unknown features page.");
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("Received acknowledge request for unknown feature.");

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }

        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        warn!("virtio-iommu device configuration is read-only");
    }

    fn activate(
        &mut self,
        mem: Arc<RwLock<GuestMemoryMmap>>,
        interrupt_cb: Arc<VirtioInterrupt>,
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != NUM_QUEUES || queue_evts.len() != NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        let (self_kill_evt, kill_evt) =
            match EventFd::new(EFD_NONBLOCK).and_then(|e| Ok((e.try_clone()?, e))) {
                Ok(v) => v,
                Err(e) => {
                    error!("failed creating kill EventFd pair: {}", e);
                    return Err(ActivateError::BadActivate);
                }
            };
        self.kill_evt = Some(self_kill_evt);

        // Save the interrupt EventFD as we need to return it on reset
        // but clone it to pass into the thread.
        self.interrupt_cb = Some(interrupt_cb.clone());

        let mut tmp_queue_evts: Vec<EventFd> = Vec::new();
        for queue_evt in queue_evts.iter() {
            // Save the queue EventFD as we need to return it on reset
            // but clone it to pass into the thread.
            tmp_queue_evts.push(queue_evt.try_clone().map_err(|e| {
                error!("failed to clone queue EventFd: {}", e);
                ActivateError::BadActivate
            })?);
        }
        self.queue_evts = Some(tmp_queue_evts);

        let mut handler = IommuEpollHandler {
            queues,
            mem,
            interrupt_cb,
            queue_evts,
            kill_evt,
            mapping: self.mapping.clone(),
            ext_mapping: self.ext_mapping.clone(),
            ext_domain_mapping: BTreeMap::new(),
        };

        let worker_result = thread::Builder::new()
            .name("virtio-iommu".to_string())
            .spawn(move || handler.run());

        if let Err(e) = worker_result {
            error!("failed to spawn virtio-iommu worker: {}", e);
            return Err(ActivateError::BadActivate);
        }

        Ok(())
    }

    fn reset(&mut self) -> Option<(Arc<VirtioInterrupt>, Vec<EventFd>)> {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }

        // Return the interrupt and queue EventFDs
        Some((
            self.interrupt_cb.take().unwrap(),
            self.queue_evts.take().unwrap(),
        ))
    }
}
