/* Original TheKernel ABI query, Apache-2.0. */
#include "acpi.h"
unsigned int tk_acpi_abi_width(void) { return sizeof(ACPI_SIZE) * 8; }
