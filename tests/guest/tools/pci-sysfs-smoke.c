#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <unistd.h>

/* MSI (cap 0x05, control bit 0) or MSI-X (cap 0x11, control bit 15) enabled. */
static int msi_enabled(const unsigned char *config) {
    if (!(config[6] & 0x10)) return 0;
    unsigned pos=config[0x34]&0xfc, guard=0;
    while (pos>=0x40 && pos<0xff && guard++<48) {
        unsigned id=config[pos], control=config[pos+2]|config[pos+3]<<8;
        if (id==0x05 && (control&1)) return 1;
        if (id==0x11 && (control&0x8000)) return 1;
        pos=config[pos+1]&0xfc;
    }
    return 0;
}

static int fail(const char *label) { fprintf(stderr, "PCI sysfs: %s (errno=%d)\n", label, errno); return 1; }
static int resources(const char *path, const unsigned char *config) {
    FILE *file=fopen(path,"r"); if (!file) return fail("resource-open");
    char line[256]; unsigned rows=0;
    while (fgets(line,sizeof(line),file)) {
        unsigned long long start,end,flags; char extra;
        if (rows>=6 || sscanf(line,"0x%16llx 0x%16llx 0x%16llx %c",&start,&end,&flags,&extra)!=3 ||
            strlen(line)!=57 || line[56]!='\n') return fail("resource-prefix-grammar");
        if (flags) {
            unsigned raw; memcpy(&raw, config+0x10+rows*4, sizeof(raw));
            unsigned long long expected=raw & ((flags&0x100) ? ~3U : ~15U);
            if (!(flags&0x100) && (raw&6)==4 && rows<5) { unsigned high;memcpy(&high,config+0x14+rows*4,sizeof(high));expected|=(unsigned long long)high<<32; }
            if (start!=expected || end<start || !(flags&0x300)) return fail("resource-config-range");
        } else if (start || end) return fail("resource-absent-not-invented");
        rows++;
    }
    if (ferror(file) || fclose(file)) return fail("resource-read");
    // Empty means not observed by the native boot BAR owner, not no resources.
    return 0;
}
static int read_hex(const char *path, unsigned *value) {
    FILE *file=fopen(path,"r"); if (!file) return -1;
    int result=fscanf(file,"%x",value)==1 ? 0 : -1; fclose(file); return result;
}
int main(void) {
    DIR *directory=opendir("/sys/bus/pci/devices");
    if (!directory) return fail("inventory-open");
    struct dirent *entry;
    unsigned count=0, bridges=0;
    char retained[512]={0}; int retained_fd=-1; ssize_t privileged_count=0;
    while ((entry=readdir(directory))) {
        if (entry->d_name[0]=='.') continue;
        char path[512]; unsigned vendor,device,class_id;
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/vendor",entry->d_name);
        if (read_hex(path,&vendor) || vendor==0xffff) return fail("vendor");
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/device",entry->d_name);
        if (read_hex(path,&device)) return fail("device");
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/class",entry->d_name);
        if (read_hex(path,&class_id)) return fail("class");
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/config",entry->d_name);
        int fd=open(path,O_RDONLY); unsigned char config[512];
        ssize_t bytes=fd<0 ? -1 : pread(fd,config,sizeof(config),0);
        struct stat metadata;
        if (fd<0 || fstat(fd,&metadata) || (metadata.st_size!=256 && metadata.st_size!=4096)) return fail("config-metadata-length");
        if (bytes<256 || (unsigned)(config[0]|config[1]<<8)!=vendor ||
            (unsigned)(config[2]|config[3]<<8)!=device ||
            (unsigned)(config[9]|config[10]<<8|config[11]<<16)!=class_id) return fail("config-identity");
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/resource",entry->d_name);
        if (resources(path, config)) return 1;
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/irq",entry->d_name);
        FILE *irq_file=fopen(path,"r"); unsigned irq;
        if (!irq_file || fscanf(irq_file,"%u",&irq)!=1 || irq>=0xf0) return fail("irq-vector");
        fclose(irq_file);
        // Like Linux, a function whose driver enabled MSI/MSI-X reports that
        // vector in `irq`; only INTx/no-interrupt functions keep the firmware
        // route. Primary MSI message selection is covered on the host.
        if (msi_enabled(config)) {
            if (irq==0) return fail("irq-msi-vector");
        } else {
            unsigned expected_irq=config[0x3d] && config[0x3c]<0xd0 ? 0x20+config[0x3c] : 0;
            if (irq!=expected_irq) return fail("irq-firmware-route");
        }
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/config",entry->d_name);
        if ((class_id >> 8)==0x0604) bridges++;
        if (retained_fd<0 && (config[14]&0x7f)==0) {
            snprintf(retained,sizeof(retained),"%s",path);
            retained_fd=fd; privileged_count=bytes;
        } else close(fd);
        count++;
    }
    closedir(directory);
    if (!count || !bridges || retained_fd<0) return fail("inventory-and-bridges");
    int writable=open(retained,O_RDWR);
    if (writable>=0) {
        unsigned char before,after,attempted;
        if (pread(writable,&before,1,0)!=1) return fail("readonly-before");
        attempted=before^0x5a;
        if (pwrite(writable,&attempted,1,0)!=-1 || pread(writable,&after,1,0)!=1 || before!=after)
            return fail("config-must-not-write");
        close(writable);
    } else if (errno!=EACCES && errno!=EROFS) return fail("readonly-open");
    pid_t child=fork();
    if (child<0) return fail("fork");
    if (!child) {
        if (setgid(65534) || setuid(65534)) _exit(2);
        unsigned char bytes[512];
        if (pread(retained_fd,bytes,sizeof(bytes),0)!=privileged_count) _exit(3);
        int fd=open(retained,O_RDONLY);
        struct stat privileged_metadata, unprivileged_metadata;
        if (fd<0 || pread(fd,bytes,sizeof(bytes),0)!=64 ||
            fstat(retained_fd,&privileged_metadata) || fstat(fd,&unprivileged_metadata) ||
            privileged_metadata.st_size!=unprivileged_metadata.st_size) _exit(4);
        close(fd);
        fd=open(retained,O_WRONLY);
        if (fd>=0 || errno!=EACCES) _exit(5);
        _exit(0);
    }
    int status;
    if (waitpid(child,&status,0)!=child || !WIFEXITED(status) || WEXITSTATUS(status)) return fail("open-credential-and-prefix");
    close(retained_fd);
    printf("THEKERNEL_PCI_SYSFS_OK functions=%u bridges=%u\n",count,bridges);
    return 0;
}
