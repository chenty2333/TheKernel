/* Original TheKernel regression, Apache-2.0; not OEM firmware. */
DefinitionBlock ("", "SSDT", 2, "TKTEST", "THERMAL", 1)
{
    Scope (\_TZ)
    {
        ThermalZone (TKTZ)
        {
            Name (STRT, Zero)
            Name (_CRT, 3100)
            Name (_PSV, 3050)
            Method (_TMP, 0, Serialized)
            {
                If (STRT == Zero) { Store (Timer (), STRT) }
                If ((Timer () - STRT) >= 100000000) { Return (3100) }
                Return (3000)
            }
        }
    }
}
