/* Original TheKernel regression, Apache-2.0; not OEM firmware. */
DefinitionBlock ("", "SSDT", 2, "TKTEST", "BUTTON", 1)
{
    Scope (\_SB)
    {
        Device (TKPB)
        {
            Name (_HID, EisaId ("PNP0C0C"))
            Name (_STA, 0x0F)
            Name (_PRW, Package () { 3, 0 })
            Method (_INI, 0) { Sleep (3000) Notify (TKPB, 0x80) }
        }
    }
}
