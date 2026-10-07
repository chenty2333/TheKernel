/* Original TheKernel fixture; contains no manufacturer firmware. Apache-2.0. */
DefinitionBlock ("", "DSDT", 2, "TKTEST", "SYNTH", 1)
{
    Name (_S5, Package () { 5, 5 })
    Scope (_SB)
    {
        Device (PCI0)
        {
            Name (_HID, "PNP0A08")
            Name (_PRT, Package () { Package () { 0xFFFF, 0, 0, 16 } })
        }
        Device (PWRB)
        {
            Name (_HID, "PNP0C0C")
            Method (_STA, 0) { Return (0x0F) }
            Method (TEST, 0) { Notify (PWRB, 0x80) }
        }
    }
}
