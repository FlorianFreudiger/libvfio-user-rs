#[macro_use]
extern crate derive_builder;

use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

use anyhow::anyhow;

use libvfio_user_sys::*;

mod callbacks;
pub mod dma;
mod setup;

#[derive(Clone, Debug)]
pub enum PciType {
    Pci,
    PciX1,
    PciX2,
    PciExpress,
}

impl PciType {
    fn to_vfu_type(&self) -> vfu_dev_type_t {
        match self {
            PciType::Pci => vfu_pci_type_t_VFU_PCI_TYPE_CONVENTIONAL,
            PciType::PciX1 => vfu_pci_type_t_VFU_PCI_TYPE_PCI_X_1,
            PciType::PciX2 => vfu_pci_type_t_VFU_PCI_TYPE_PCI_X_2,
            PciType::PciExpress => vfu_pci_type_t_VFU_PCI_TYPE_EXPRESS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PciConfig {
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    pub class_code_base: u8,
    pub class_code_subclass: u8,
    pub class_code_programming_interface: u8,
    pub revision_id: u8,
}

#[derive(Clone, Debug)]
pub struct DeviceRegion {
    pub region_type: DeviceRegionKind,
    pub size: usize,
    pub file_descriptor: i32,
    pub offset: u64,
    pub read: bool,
    pub write: bool,
    pub memory: bool,
}

#[derive(Clone, Debug)]
pub enum DeviceRegionKind {
    Bar0,
    Bar1,
    Bar2,
    Bar3,
    Bar4,
    Bar5,
    Rom,
    Config { always_callback: bool },
    Vga,
    Migration,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum InterruptRequestKind {
    /// Legacy interrupt
    IntX,
    /// Message signaled interrupt
    Msi,
    MsiX,
    // Other
    Err,
    Req,
}

impl InterruptRequestKind {
    fn to_vfu_type(&self) -> vfu_dev_irq_type {
        match self {
            InterruptRequestKind::IntX => vfu_dev_irq_type_VFU_DEV_INTX_IRQ,
            InterruptRequestKind::Msi => vfu_dev_irq_type_VFU_DEV_MSI_IRQ,
            InterruptRequestKind::MsiX => vfu_dev_irq_type_VFU_DEV_MSIX_IRQ,
            InterruptRequestKind::Err => vfu_dev_irq_type_VFU_DEV_ERR_IRQ,
            InterruptRequestKind::Req => vfu_dev_irq_type_VFU_DEV_REQ_IRQ,
        }
    }
}

#[derive(Clone, Debug)]
pub enum DeviceResetReason {
    ClientRequest,
    LostConnection,
    PciReset,
}

#[derive(Builder, Debug)]
#[builder(name = "DeviceConfigurator", build_fn(validate = "Self::validate"))]
pub struct DeviceConfiguration {
    // Path to the socket to be used for communication with the client (e.g. qemu)
    socket_path: PathBuf,

    // Remove socket before setup if it already exists
    #[builder(default = "false")]
    overwrite_socket: bool,

    // Run non-blocking, caller must handle waiting/polling for requests itself
    #[builder(default = "false")]
    non_blocking: bool,

    // Type of PCI connector the vfio-user client should expose
    #[builder(default = "PciType::Pci")]
    pci_type: PciType,

    // Exposed PCI information
    pci_config: PciConfig,

    #[builder(setter(custom))]
    device_regions: Vec<DeviceRegion>,

    #[builder(setter(custom))]
    interrupt_request_counts: HashMap<InterruptRequestKind, u32>,

    #[builder(default = "false")]
    setup_dma: bool,
}

impl DeviceConfigurator {
    pub fn add_device_region(&mut self, region: DeviceRegion) -> &mut Self {
        self.device_regions.get_or_insert(Vec::new()).push(region);
        self
    }

    pub fn using_interrupt_requests(
        &mut self, irq_kind: InterruptRequestKind, count: u32,
    ) -> &mut Self {
        self.interrupt_request_counts
            .get_or_insert(HashMap::new())
            .insert(irq_kind, count);
        self
    }
}

impl DeviceConfiguration {
    pub fn produce<T: Device>(&self) -> anyhow::Result<Box<T>> {
        unsafe { self.setup_all() }
    }
}

#[derive(Debug)]
pub struct DeviceContext {
    vfu_ctx: *mut vfu_ctx_t,
    dma_enabled: bool,
}

impl DeviceContext {
    /// Attach to the transport, if non-blocking it may return None and needs to be called again
    pub fn attach(&self) -> anyhow::Result<Option<()>> {
        unsafe {
            let ret = vfu_attach_ctx(self.vfu_ctx);

            if ret != 0 {
                let err = Error::last_os_error();

                return if err.kind() == ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(anyhow!("Failed to attach device: {}", err))
                };
            }

            Ok(Some(()))
        }
    }

    pub fn run(&self) -> anyhow::Result<()> {
        unsafe {
            // Loop until all requests have been processed, useful for non-blocking contexts.
            // If blocking, can only return via error or client disconnect, regardless of the loop
            loop {
                let processed_requests = vfu_run_ctx(self.vfu_ctx);

                if processed_requests < 0 {
                    let err = Error::last_os_error();
                    return Err(anyhow!("Failed to run device: {}", err));
                }

                if processed_requests == 0 {
                    break;
                }
            }

            Ok(())
        }
    }

    pub fn trigger_irq(&self, subindex: u32) -> anyhow::Result<()> {
        unsafe {
            let ret = vfu_irq_trigger(self.vfu_ctx, subindex);

            if ret != 0 {
                let err = Error::last_os_error();
                return Err(anyhow!("Failed to trigger irq: {}", err));
            }

            Ok(())
        }
    }
}

impl Drop for DeviceContext {
    fn drop(&mut self) {
        unsafe {
            vfu_destroy_ctx(self.vfu_ctx);
        }
    }
}

impl AsRawFd for DeviceContext {
    fn as_raw_fd(&self) -> RawFd {
        unsafe { vfu_get_poll_fd(self.vfu_ctx) }
    }
}

#[allow(unused_variables)]
pub trait Device {
    fn new(ctx: DeviceContext) -> Self;
    fn ctx(&self) -> &DeviceContext;
    fn ctx_mut(&mut self) -> &mut DeviceContext;

    fn log(&self, level: i32, msg: &str);

    fn reset(&mut self, reason: DeviceResetReason) -> Result<(), i32>;

    fn region_access_bar0(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar1(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar2(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar3(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar4(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_bar5(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_rom(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_config(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_vga(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    fn region_access_migration(
        &mut self, offset: usize, data: &mut [u8], write: bool,
    ) -> Result<usize, i32> {
        unimplemented!()
    }

    // Optional dma callbacks, regions are also automatically tracked in DeviceContext's dma_regions
    fn dma_range_added(&mut self, base_address: usize, length: usize) {}
    fn dma_range_removed(&mut self, base_address: usize) {}
}
