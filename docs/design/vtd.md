# Intel VT-d starting point

`tk-vtd` parses the ACPI DMAR fixed header and DRHD/RMRR records, validates
structure lengths and alignment, selects direct endpoint scopes before an
include-all unit, and defines an explicit DMA map/unmap contract. Disabled
IOMMU operation is identity; enabled operation delegates to a backend and
never silently falls back to identity when mapping fails. There is not yet a
hardware backend, page-table/context-root/QI/fault/interruption machinery or
boot integration, so the product does not enable VT-d through this crate yet.
virtio/NVMe continue using their existing DMA interfaces and are not claimed
to be translated through VT-d.
