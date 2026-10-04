#define _GNU_SOURCE
#include <elf.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <unistd.h>

int main(void) {
    int fd = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    if (fd < 0) { perror("exe"); return 1; }
    Elf64_Ehdr header;
    if (pread(fd, &header, sizeof(header), 0) != sizeof(header) ||
        memcmp(header.e_ident, ELFMAG, SELFMAG) || header.e_ident[EI_CLASS] != ELFCLASS64 ||
        header.e_phentsize != sizeof(Elf64_Phdr) || !header.e_phnum || header.e_phnum > 128) return 1;
    uint64_t bias = header.e_type == ET_DYN ? getauxval(AT_ENTRY)-header.e_entry : 0;
    uint64_t code_start = UINT64_MAX, code_end = 0, data_start = 0, data_end = 0;
    for (unsigned int i = 0; i < header.e_phnum; i++) {
        Elf64_Phdr ph;
        if (pread(fd, &ph, sizeof(ph), (off_t)header.e_phoff+(off_t)i*sizeof(ph)) != sizeof(ph)) return 1;
        if (ph.p_type != PT_LOAD) continue;
        uint64_t start = bias+ph.p_vaddr, end = start+ph.p_filesz;
        if (ph.p_flags & PF_X) {
            if (start < code_start) code_start = start;
            if (end > code_end) code_end = end;
        }
        if (start > data_start) data_start = start;
        if (end > data_end) data_end = end;
    }
    if (close(fd)) return 1;
    FILE *stat = fopen("/proc/self/stat", "r");
    if (!stat) return 1;
    char line[8192];
    if (!fgets(line, sizeof(line), stat) || fclose(stat)) return 1;
    char *tail = strrchr(line, ')');
    if (!tail) return 1;
    char *save;
    char *token = strtok_r(tail+1, " \t\n", &save);
    uint64_t observed[4] = {0};
    for (unsigned int field = 3; token; field++, token = strtok_r(NULL, " \t\n", &save)) {
        if (field == 26) observed[0] = strtoull(token, NULL, 10);
        if (field == 27) observed[1] = strtoull(token, NULL, 10);
        if (field == 45) observed[2] = strtoull(token, NULL, 10);
        if (field == 46) observed[3] = strtoull(token, NULL, 10);
    }
    if (observed[0] != code_start || observed[1] != code_end ||
        observed[2] != data_start || observed[3] != data_end) {
        fprintf(stderr, "ELF_MM_BOUNDS_FAIL observed=%llx/%llx/%llx/%llx expected=%llx/%llx/%llx/%llx\n",
                (unsigned long long)observed[0],(unsigned long long)observed[1],
                (unsigned long long)observed[2],(unsigned long long)observed[3],
                (unsigned long long)code_start,(unsigned long long)code_end,
                (unsigned long long)data_start,(unsigned long long)data_end);
        return 1;
    }
    puts("ELF_MM_BOUNDS_OK");
    return 0;
}
