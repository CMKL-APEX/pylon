// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

//! `pylon-kick`: the PCI device that delivers Pylon revocations to the guest as interrupts
//! (`spec/06-guest-kick-path.md` §4.2).
//!
//! Each vCPU has one MSI-X vector. Its notifier is the vCPU's kick `EventFd` from
//! [`PylonVmmState`], which the VMM already hands to `pylond` at registration, so a kick from
//! `pylond` raises the vector once the guest driver has unmasked it. The `EventFd`s outlive the
//! device: a rebooted VM gets a new device with the same notifiers, and `pylond` keeps the ones
//! it has. While the guest hasn't enabled a vector, or has masked it, a kick waits in the
//! `EventFd`, and KVM injects it once the vector is enabled.
//!
//! BAR0 holds the read-only identification registers and the park doorbell. BAR1 holds the
//! MSI-X table and pending bit array.

use std::any::Any;
use std::sync::{Arc, Barrier, Mutex};
use std::{io, result};

use anyhow::anyhow;
use log::{debug, error};
use pci::{
    BarReprogrammingParams, MSIX_TABLE_ENTRY_SIZE, MaybeMutInterruptSourceGroup, MsixCap,
    MsixConfig, PciBarConfiguration, PciBarPrefetchable, PciBarRegionType, PciBdf, PciClassCode,
    PciConfiguration, PciDevice, PciDeviceError, PciHeaderType, PciSubclass,
};
use pylon_abi::{
    MAX_VCPUS, PYLON_ABI_VERSION, PYLON_KICK_BAR0_SIZE, PYLON_KICK_DEVICE_ID,
    PYLON_KICK_REG_ABI_VERSION, PYLON_KICK_REG_MAGIC, PYLON_KICK_REG_NUM_VCPUS,
    PYLON_KICK_REG_PARK_DOORBELL, PYLON_KICK_REG_SHM_GPA_HI, PYLON_KICK_REG_SHM_GPA_LO,
    PYLON_KICK_VENDOR_ID, PYLON_MAGIC,
};
use thiserror::Error;
use vm_allocator::{AddressAllocator, SystemAllocator};
use vm_device::interrupt::{
    InterruptIndex, InterruptManager, InterruptSourceConfig, InterruptSourceGroup,
    MsiIrqGroupConfig, MsiIrqSourceConfig,
};
use vm_device::{BusDevice, Resource};
use vm_memory::{Address, GuestAddress};
use vm_migration::{Migratable, MigratableError, Pausable, Snapshot, Snapshottable, Transportable};

use crate::pylon::PylonVmmState;

/// BAR index of the identification registers and the park doorbell.
const REGISTERS_BAR: usize = 0;
/// BAR index of the MSI-X table and pending bit array.
const MSIX_BAR: usize = 1;
/// Size of the MSI-X BAR: the table in the first half, the pending bit array in the second.
const MSIX_BAR_SIZE: u64 = 0x1000;
const MSIX_TABLE_OFFSET: u64 = 0;
const MSIX_PBA_OFFSET: u64 = MSIX_BAR_SIZE / 2;

// One table entry per vCPU has to fit in the table's half of the BAR.
const _: () = assert!(MAX_VCPUS * MSIX_TABLE_ENTRY_SIZE <= MSIX_PBA_OFFSET as usize);

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to create the pylon-kick MSI-X interrupt group")]
    CreateInterruptGroup(#[source] io::Error),

    #[error("Failed to install the kick EventFd of vCPU {0} as its MSI-X notifier")]
    SetNotifier(u32, #[source] io::Error),
}

#[derive(Copy, Clone)]
enum PylonKickSubclass {
    Other = 0x80,
}

impl PciSubclass for PylonKickSubclass {
    fn get_register_value(&self) -> u8 {
        *self as u8
    }
}

/// The `pylon-kick` PCI device.
pub struct PylonKickDevice {
    id: String,
    bdf: PciBdf,
    num_vcpus: u32,
    shm_gpa: u64,
    configuration: PciConfiguration,
    msix_config: Arc<Mutex<MsixConfig>>,
    interrupt_group: MaybeMutInterruptSourceGroup,
    bar_regions: Vec<PciBarConfiguration>,
}

impl PylonKickDevice {
    /// Creates the device for `state`'s VM. Its MSI-X vector *i* is raised by writing vCPU
    /// *i*'s kick `EventFd`.
    pub fn create(
        id: String,
        bdf: PciBdf,
        state: &PylonVmmState,
        interrupt_manager: &dyn InterruptManager<GroupConfig = MsiIrqGroupConfig>,
        vm: &dyn hypervisor::Vm,
    ) -> result::Result<Self, Error> {
        let num_vcpus = state.max_vcpus();
        let group = interrupt_manager
            .create_group_mut(MsiIrqGroupConfig {
                base: 0,
                count: num_vcpus as InterruptIndex,
            })
            .map_err(Error::CreateInterruptGroup)?;
        {
            let mut group = group.lock().unwrap();
            for vcpu_id in 0..num_vcpus {
                let kick = state
                    .kick_eventfd(vcpu_id)
                    .map(|fd| fd.try_clone())
                    .transpose()
                    .map_err(|e| Error::SetNotifier(vcpu_id, e))?;
                group
                    .set_notifier(vcpu_id as InterruptIndex, kick, vm)
                    .map_err(|e| Error::SetNotifier(vcpu_id, e))?;
            }
        }
        Ok(Self::new(
            id,
            bdf,
            num_vcpus,
            state.config().gpa,
            MaybeMutInterruptSourceGroup::Mutable(group),
        ))
    }

    fn new(
        id: String,
        bdf: PciBdf,
        num_vcpus: u32,
        shm_gpa: u64,
        interrupt_group: MaybeMutInterruptSourceGroup,
    ) -> Self {
        let msix_config = Arc::new(Mutex::new(
            MsixConfig::new(num_vcpus as u16, interrupt_group.clone(), bdf.into(), None)
                .expect("MsixConfig::new only fails when restoring a saved state"),
        ));
        let configuration = PciConfiguration::new(
            PYLON_KICK_VENDOR_ID,
            PYLON_KICK_DEVICE_ID,
            0x1,
            PciClassCode::BaseSystemPeripheral,
            &PylonKickSubclass::Other,
            None,
            PciHeaderType::Device,
            PYLON_KICK_VENDOR_ID,
            PYLON_KICK_DEVICE_ID,
            Some(Arc::clone(&msix_config)),
            None,
        );

        PylonKickDevice {
            id,
            bdf,
            num_vcpus,
            shm_gpa,
            configuration,
            msix_config,
            interrupt_group,
            bar_regions: Vec::new(),
        }
    }

    /// Returns the BAR index whose current base address is `base`.
    fn bar_at(&self, base: u64) -> Option<usize> {
        self.bar_regions
            .iter()
            .find(|bar| bar.addr() == base)
            .map(PciBarConfiguration::idx)
    }

    fn register(&self, offset: u64) -> u32 {
        match offset {
            PYLON_KICK_REG_MAGIC => PYLON_MAGIC as u32,
            PYLON_KICK_REG_ABI_VERSION => PYLON_ABI_VERSION,
            PYLON_KICK_REG_NUM_VCPUS => self.num_vcpus,
            PYLON_KICK_REG_SHM_GPA_LO => self.shm_gpa as u32,
            PYLON_KICK_REG_SHM_GPA_HI => (self.shm_gpa >> 32) as u32,
            _ => 0,
        }
    }

    fn read_registers(&self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let offset = offset + i as u64;
            *byte = self.register(offset & !3).to_le_bytes()[(offset & 3) as usize];
        }
    }

    /// Writes the MSI-X table, and takes a vector's irqfd out of KVM when the guest masks it.
    ///
    /// `MsixConfig` only reroutes unmasked vectors: it relies on a device to check the mask
    /// before it triggers one. `pylond` writes the kick `EventFd` directly, so a masked vector
    /// would still fire. Without its irqfd, a kick waits in the `EventFd`; the guest's next
    /// unmask puts the irqfd back, and KVM then delivers it.
    fn write_msix_table(&self, offset: u64, data: &[u8]) {
        let mut msix = self.msix_config.lock().unwrap();
        let index = (offset / MSIX_TABLE_ENTRY_SIZE as u64) as usize;
        let was_masked = msix.table_entries.get(index).map(|entry| entry.masked());
        msix.write_table(offset, data);

        let entry = &msix.table_entries[..];
        if was_masked == Some(false) && entry[index].masked() && msix.enabled() && !msix.masked() {
            let config = MsiIrqSourceConfig {
                high_addr: entry[index].msg_addr_hi,
                low_addr: entry[index].msg_addr_lo,
                data: entry[index].msg_data,
                devid: msix.devid,
            };
            if let Err(e) = self.interrupt_group.update(
                index as InterruptIndex,
                InterruptSourceConfig::MsiIrq(config),
                true,
                true,
            ) {
                error!("pylon-kick: failed to mask MSI-X vector {index}: {e}");
            }
        }
    }

    fn write_registers(&self, offset: u64, data: &[u8]) {
        // The exit that brought the write here is the doorbell; the post-exit hook in
        // `vmm/src/cpu.rs` parks the vCPU. Blocking here would hold the device's lock and
        // serialize every vCPU that touches the device (spec 06 §4.3).
        if offset == PYLON_KICK_REG_PARK_DOORBELL {
            let mut value = [0u8; 4];
            let len = data.len().min(value.len());
            value[..len].copy_from_slice(&data[..len]);
            debug!(
                "pylon-kick: park doorbell from vCPU {}",
                u32::from_le_bytes(value)
            );
        }
    }
}

impl BusDevice for PylonKickDevice {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        self.read_bar(base, offset, data);
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        self.write_bar(base, offset, data)
    }
}

impl PciDevice for PylonKickDevice {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> (Vec<BarReprogrammingParams>, Option<Arc<Barrier>>) {
        (
            self.configuration
                .write_config_register(reg_idx, offset, data),
            None,
        )
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        self.configuration.read_reg(reg_idx)
    }

    fn allocate_bars(
        &mut self,
        _allocator: &mut SystemAllocator,
        mmio32_allocator: &mut AddressAllocator,
        _mmio64_allocator: &mut AddressAllocator,
        _resources: Option<Vec<Resource>>,
    ) -> result::Result<Vec<PciBarConfiguration>, PciDeviceError> {
        let mut bars = Vec::new();
        for (idx, size) in [
            (REGISTERS_BAR, PYLON_KICK_BAR0_SIZE),
            (MSIX_BAR, MSIX_BAR_SIZE),
        ] {
            let addr = mmio32_allocator
                .allocate(None, size, Some(size))
                .ok_or(PciDeviceError::IoAllocationFailed(size))?;
            let bar = PciBarConfiguration::default()
                .set_index(idx)
                .set_address(addr.raw_value())
                .set_size(size)
                .set_region_type(PciBarRegionType::Memory32BitRegion)
                .set_prefetchable(PciBarPrefetchable::NotPrefetchable);
            self.configuration
                .add_pci_bar(&bar)
                .map_err(|e| PciDeviceError::IoRegistrationFailed(addr.raw_value(), e))?;
            bars.push(bar);
        }

        let msix_cap = MsixCap::new(
            MSIX_BAR as u8,
            self.num_vcpus as u16,
            MSIX_TABLE_OFFSET as u32,
            MSIX_BAR as u8,
            MSIX_PBA_OFFSET as u32,
        );
        self.configuration
            .add_capability(&msix_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        self.bar_regions.clone_from(&bars);
        Ok(bars)
    }

    fn free_bars(
        &mut self,
        _allocator: &mut SystemAllocator,
        mmio32_allocator: &mut AddressAllocator,
        _mmio64_allocator: &mut AddressAllocator,
    ) -> result::Result<(), PciDeviceError> {
        for bar in self.bar_regions.drain(..) {
            mmio32_allocator.free(GuestAddress(bar.addr()), bar.size());
        }
        Ok(())
    }

    fn move_bar(&mut self, bar_idx: usize, new_base: u64) -> io::Result<()> {
        for bar in self.bar_regions.iter_mut() {
            if bar.idx() == bar_idx {
                *bar = bar.set_address(new_base);
            }
        }
        Ok(())
    }

    fn restore_bar_addr(&mut self, params: &BarReprogrammingParams) {
        self.configuration.restore_bar_addr(params);
    }

    fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        match self.bar_at(base) {
            Some(REGISTERS_BAR) => self.read_registers(offset, data),
            Some(MSIX_BAR) if offset < MSIX_PBA_OFFSET => self
                .msix_config
                .lock()
                .unwrap()
                .read_table(offset - MSIX_TABLE_OFFSET, data),
            Some(MSIX_BAR) => self
                .msix_config
                .lock()
                .unwrap()
                .read_pba(offset - MSIX_PBA_OFFSET, data),
            _ => data.fill(0),
        }
    }

    fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        match self.bar_at(base) {
            Some(REGISTERS_BAR) => self.write_registers(offset, data),
            Some(MSIX_BAR) if offset < MSIX_PBA_OFFSET => {
                self.write_msix_table(offset - MSIX_TABLE_OFFSET, data);
            }
            Some(MSIX_BAR) => self
                .msix_config
                .lock()
                .unwrap()
                .write_pba(offset - MSIX_PBA_OFFSET, data),
            _ => {}
        }
        None
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn bdf(&self) -> PciBdf {
        self.bdf
    }
}

impl Pausable for PylonKickDevice {}

impl Snapshottable for PylonKickDevice {
    fn id(&self) -> String {
        self.id.clone()
    }

    /// Fails: the shared page and the `pylond` registration have no snapshot or migration
    /// story yet (spec 06 §7), so a VM with `--pylon` refuses to snapshot or migrate.
    fn snapshot(&mut self) -> result::Result<Snapshot, MigratableError> {
        Err(MigratableError::Snapshot(anyhow!(
            "VMs with --pylon can't be snapshotted or migrated"
        )))
    }
}

impl Transportable for PylonKickDevice {}
impl Migratable for PylonKickDevice {}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "x86_64")]
    use vm_allocator::GsiApic;
    use vm_device::interrupt::{InterruptSourceConfig, InterruptSourceGroup};
    use vmm_sys_util::eventfd::EventFd;

    use super::*;

    const NUM_VCPUS: u32 = 4;
    const SHM_GPA: u64 = 0x1_FEDF_0000;

    struct TestInterruptSourceGroup;

    impl InterruptSourceGroup for TestInterruptSourceGroup {
        fn trigger(&self, _index: InterruptIndex) -> io::Result<()> {
            Ok(())
        }
        fn notifier(&self, _index: InterruptIndex) -> Option<EventFd> {
            None
        }
        fn update(
            &self,
            _index: InterruptIndex,
            _config: InterruptSourceConfig,
            _masked: bool,
            _set_gsi: bool,
        ) -> io::Result<()> {
            Ok(())
        }
        fn set_gsi(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Returns a device with its BARs allocated, and the BARs' base addresses.
    fn device() -> (PylonKickDevice, u64, u64) {
        let mut device = PylonKickDevice::new(
            "kick".to_string(),
            PciBdf::new(0, 0, 5, 0),
            NUM_VCPUS,
            SHM_GPA,
            MaybeMutInterruptSourceGroup::Immutable(Arc::new(TestInterruptSourceGroup)),
        );
        let mut allocator = SystemAllocator::new(
            GuestAddress(0x1000_0000),
            0x1000_0000,
            GuestAddress(0x2000_0000),
            0x1000_0000,
            #[cfg(target_arch = "x86_64")]
            &[GsiApic::new(5, 19)],
        )
        .unwrap();
        let mut mmio32 = AddressAllocator::new(GuestAddress(0xc000_0000), 0x100_0000).unwrap();
        let mut mmio64 = AddressAllocator::new(GuestAddress(0x1_0000_0000), 0x100_0000).unwrap();
        let bars = device
            .allocate_bars(&mut allocator, &mut mmio32, &mut mmio64, None)
            .unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].idx(), REGISTERS_BAR);
        assert_eq!(bars[1].idx(), MSIX_BAR);
        (device, bars[0].addr(), bars[1].addr())
    }

    fn read_u32(device: &mut PylonKickDevice, base: u64, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        device.read_bar(base, offset, &mut data);
        u32::from_le_bytes(data)
    }

    #[test]
    fn registers_identify_the_device_and_the_shared_page() {
        let (mut device, regs, _) = device();

        assert_eq!(
            read_u32(&mut device, regs, PYLON_KICK_REG_MAGIC),
            PYLON_MAGIC as u32
        );
        assert_eq!(
            read_u32(&mut device, regs, PYLON_KICK_REG_ABI_VERSION),
            PYLON_ABI_VERSION
        );
        assert_eq!(
            read_u32(&mut device, regs, PYLON_KICK_REG_NUM_VCPUS),
            NUM_VCPUS
        );
        assert_eq!(
            read_u32(&mut device, regs, PYLON_KICK_REG_SHM_GPA_LO),
            0xFEDF_0000
        );
        assert_eq!(read_u32(&mut device, regs, PYLON_KICK_REG_SHM_GPA_HI), 1);
        assert_eq!(read_u32(&mut device, regs, PYLON_KICK_REG_PARK_DOORBELL), 0);
        assert_eq!(read_u32(&mut device, regs, 0x800), 0);

        // Narrower and wider reads see the same little-endian registers.
        let mut byte = [0u8; 1];
        device.read_bar(regs, PYLON_KICK_REG_MAGIC + 1, &mut byte);
        assert_eq!(byte[0], (PYLON_MAGIC >> 8) as u8);
        let mut gpa = [0u8; 8];
        device.read_bar(regs, PYLON_KICK_REG_SHM_GPA_LO, &mut gpa);
        assert_eq!(u64::from_le_bytes(gpa), SHM_GPA);
    }

    #[test]
    fn doorbell_writes_change_nothing() {
        let (mut device, regs, _) = device();

        assert!(
            device
                .write_bar(regs, PYLON_KICK_REG_PARK_DOORBELL, &2u32.to_le_bytes())
                .is_none()
        );
        device.write_bar(regs, PYLON_KICK_REG_NUM_VCPUS, &99u32.to_le_bytes());
        assert_eq!(
            read_u32(&mut device, regs, PYLON_KICK_REG_NUM_VCPUS),
            NUM_VCPUS
        );
    }

    #[test]
    fn config_space_has_the_ids_and_an_msix_capability_in_bar1() {
        let (mut device, _, _) = device();
        let ids = u32::from(PYLON_KICK_VENDOR_ID) | (u32::from(PYLON_KICK_DEVICE_ID) << 16);
        assert_eq!(device.read_config_register(0), ids);
        // Subsystem vendor and subsystem ID, at 0x2C.
        assert_eq!(device.read_config_register(11), ids);

        // Walk the capability list (pointer at 0x34) to the MSI-X capability (ID 0x11).
        let mut cap = (device.read_config_register(13) & 0xfc) as usize;
        let msix = loop {
            assert_ne!(cap, 0, "no MSI-X capability");
            let header = device.read_config_register(cap / 4);
            if header & 0xff == 0x11 {
                break cap;
            }
            cap = ((header >> 8) & 0xfc) as usize;
        };
        let header = device.read_config_register(msix / 4);
        assert_eq!((header >> 16) & 0x7ff, NUM_VCPUS - 1, "table size");
        let table = device.read_config_register(msix / 4 + 1);
        assert_eq!(table, MSIX_TABLE_OFFSET as u32 | MSIX_BAR as u32);
        let pba = device.read_config_register(msix / 4 + 2);
        assert_eq!(pba, MSIX_PBA_OFFSET as u32 | MSIX_BAR as u32);
    }

    #[test]
    fn msix_table_and_pba_live_in_bar1() {
        let (mut device, regs, msix) = device();

        // Entry 2's message address, then read it back through the BAR.
        let entry = 2 * MSIX_TABLE_ENTRY_SIZE as u64;
        device.write_bar(msix, entry, &0xFEE0_0000u32.to_le_bytes());
        assert_eq!(read_u32(&mut device, msix, entry), 0xFEE0_0000);
        assert_eq!(
            device.msix_config.lock().unwrap().table_entries[2].msg_addr_lo,
            0xFEE0_0000
        );
        // Vector control starts masked.
        assert_eq!(read_u32(&mut device, msix, entry + 12), 1);
        assert_eq!(read_u32(&mut device, msix, MSIX_PBA_OFFSET), 0);

        // The register BAR is unaffected.
        assert_eq!(
            read_u32(&mut device, regs, PYLON_KICK_REG_MAGIC),
            PYLON_MAGIC as u32
        );
    }

    /// Writes the MSI-X message control register through config space.
    #[cfg(all(target_arch = "x86_64", feature = "kvm"))]
    fn set_msix_control(device: &mut PylonKickDevice, value: u16) {
        let mut cap = (device.read_config_register(13) & 0xfc) as usize;
        while device.read_config_register(cap / 4) & 0xff != 0x11 {
            cap = ((device.read_config_register(cap / 4) >> 8) & 0xfc) as usize;
        }
        device.write_config_register(cap / 4, 2, &value.to_le_bytes());
    }

    /// A kick is the vCPU's kick `EventFd`, which `pylond` writes. While the guest hasn't
    /// enabled the vector, the kick stays in the `EventFd`; once it has, KVM's irqfd takes it,
    /// which leaves the counter at zero.
    #[cfg(all(target_arch = "x86_64", feature = "kvm"))]
    #[test]
    fn kicks_reach_kvm_once_the_guest_enables_the_vector() {
        use hypervisor::HypervisorVmConfig;
        use tempfile::TempDir;

        use crate::interrupt::MsiInterruptManager;
        use crate::pylon::PylonConfig;

        let Ok(hv) = hypervisor::new() else {
            eprintln!("Skipping real KVM test because /dev/kvm is not accessible");
            return;
        };
        let vm = hv
            .create_vm(HypervisorVmConfig::default())
            .expect("failed to create KVM VM");
        vm.create_irq_chip().unwrap();
        let allocator = Arc::new(Mutex::new(
            SystemAllocator::new(
                GuestAddress(0x1000_0000),
                0x1000_0000,
                GuestAddress(0x2000_0000),
                0x1000_0000,
                &[GsiApic::new(0, 24)],
            )
            .unwrap(),
        ));
        let manager = MsiInterruptManager::new(allocator, Arc::clone(&vm));

        let dir = TempDir::new().unwrap();
        let cfg = PylonConfig::parse(&format!("shm={}", dir.path().join("kick-pylon").display()))
            .unwrap();
        let state = PylonVmmState::new(&cfg, 2, 2).unwrap();
        let mut device = PylonKickDevice::create(
            "kick".to_string(),
            PciBdf::new(0, 0, 5, 0),
            &state,
            &manager,
            vm.as_ref(),
        )
        .unwrap();
        let mut mmio32 = AddressAllocator::new(GuestAddress(0xc000_0000), 0x100_0000).unwrap();
        let mut mmio64 = AddressAllocator::new(GuestAddress(0x1_0000_0000), 0x100_0000).unwrap();
        let mut system = SystemAllocator::new(
            GuestAddress(0x1000_0000),
            0x1000_0000,
            GuestAddress(0x2000_0000),
            0x1000_0000,
            &[GsiApic::new(0, 24)],
        )
        .unwrap();
        let bars = device
            .allocate_bars(&mut system, &mut mmio32, &mut mmio64, None)
            .unwrap();
        let msix = bars[1].addr();
        let kick = state.kick_eventfd(1).unwrap();

        // Vector 1: a fixed interrupt to vector 0x30 on APIC 0, unmasked, while MSI-X is off.
        let entry = MSIX_TABLE_ENTRY_SIZE as u64;
        device.write_bar(msix, entry, &0xFEE0_0000u32.to_le_bytes());
        device.write_bar(msix, entry + 4, &0u32.to_le_bytes());
        device.write_bar(msix, entry + 8, &0x30u32.to_le_bytes());
        device.write_bar(msix, entry + 12, &0u32.to_le_bytes());

        kick.write(1).unwrap();
        assert_eq!(
            kick.read().unwrap(),
            1,
            "a kick before MSI-X is enabled waits"
        );

        set_msix_control(&mut device, 1 << 15);
        kick.write(1).unwrap();
        assert_eq!(
            kick.read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "KVM's irqfd took the kick"
        );

        // Masking the vector leaves later kicks in the EventFd, and unmasking it hands them
        // to KVM again.
        device.write_bar(msix, entry + 12, &1u32.to_le_bytes());
        kick.write(1).unwrap();
        assert_eq!(kick.read().unwrap(), 1, "a kick to a masked vector waits");
        device.write_bar(msix, entry + 12, &0u32.to_le_bytes());
        kick.write(1).unwrap();
        assert_eq!(
            kick.read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "KVM's irqfd took the kick after the unmask"
        );

        // Disabling MSI-X takes every vector out.
        set_msix_control(&mut device, 0);
        kick.write(1).unwrap();
        assert_eq!(kick.read().unwrap(), 1, "a kick with MSI-X disabled waits");
    }

    #[test]
    fn snapshots_are_refused() {
        let (mut device, _, _) = device();
        assert!(matches!(
            Snapshottable::snapshot(&mut device),
            Err(MigratableError::Snapshot(_))
        ));
    }
}
