/* Original TheKernel failure fixture, Apache-2.0; not OEM firmware. */
DefinitionBlock ("", "SSDT", 2, "TKTEST", "PCIFAIL", 1)
{
    Scope (\_SB)
    {
        Device (TKPC)
        {
            Name (_HID, EisaId ("PNP0A08"))
            Name (_SEG, One) /* Deliberately outside admitted segment zero. */
            Name (_BBN, Zero)
        }
    }
}
