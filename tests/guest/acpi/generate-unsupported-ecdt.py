"""Original deliberately unsupported ECDT fixture (Apache-2.0), not firmware."""
from pathlib import Path
import struct
path = b"\\_SB.TKEC\0"
t = bytearray(65 + len(path))
struct.pack_into("<4sIBB6s8sI4sI", t, 0, b"ECDT", len(t), 1, 0, b"TKTEST", b"FALLEC  ", 1, b"TKER", 1)
# SystemMemory GAS is intentionally not admitted by this SystemIO EC driver.
for offset, address in ((36, 0x66), (48, 0x62)):
    struct.pack_into("<BBBBQ", t, offset, 0, 8, 0, 1, address)
t[64] = 3
t[65:] = path
t[9] = (-sum(t)) & 255
Path(__file__).with_name("unsupported-ecdt.bin").write_bytes(t)
