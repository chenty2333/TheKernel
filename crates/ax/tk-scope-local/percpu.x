CPU_NUM = 4;

/* Host tests exercise kernel text-range admission against their real text
 * section. The boot linker normally supplies these bounds. */
_stext = ADDR(.text);
_etext = ADDR(.text) + SIZEOF(.text);

SECTIONS
{
    . = ALIGN(4K);
    _percpu_start = .;
    _percpu_end = _percpu_start + SIZEOF(.percpu);
    /* Host accessors address the areas percpu::init() allocates, not this
     * image; tk_axhal::percpu::init_primary copies the image into them. Keep
     * it loaded so nonzero static initializers (notably Weak::new()) survive;
     * NOLOAD would copy invalid zero values. */
    .percpu : AT(_percpu_start) {
        _percpu_load_start = .;
        *(.percpu .percpu.*)
        _percpu_load_end = .;
        _percpu_load_end_aligned = ALIGN(64);
        . = _percpu_load_start + (_percpu_load_end_aligned - _percpu_load_start) * CPU_NUM;
    }
    . = _percpu_end;
}
INSERT AFTER .bss;
