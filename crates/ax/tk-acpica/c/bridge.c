/* Original TheKernel FFI/variadic adapter, Apache-2.0. */
#include "acpi.h"
#include "accommon.h"
extern void tk_acpi_log(const char *, ACPI_SIZE);
void AcpiOsVprintf(const char *format, va_list args) {
    char buffer[512];
    int length = vsnprintf(buffer, sizeof(buffer), format, args);
    if (length > 0) tk_acpi_log(buffer, (ACPI_SIZE)length < sizeof(buffer) ? (ACPI_SIZE)length : sizeof(buffer)-1);
}
void AcpiOsPrintf(const char *format, ...) {
    va_list args; va_start(args, format); AcpiOsVprintf(format, args); va_end(args);
}
