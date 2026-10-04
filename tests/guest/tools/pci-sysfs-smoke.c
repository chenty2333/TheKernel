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

static int fail(const char *label) { fprintf(stderr, "PCI sysfs: %s (errno=%d)\n", label, errno); return 1; }
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
        snprintf(path,sizeof(path),"/sys/bus/pci/devices/%s/irq",entry->d_name);
        FILE *irq_file=fopen(path,"r"); unsigned irq;
        if (!irq_file || fscanf(irq_file,"%u",&irq)!=1 || irq>=0xf0) return fail("irq-vector");
        fclose(irq_file);
        // All default Q35 PCI functions use disabled MSI or legacy INTx;
        // primary MSI message selection is independently covered on the host.
        unsigned expected_irq=config[0x3d] && config[0x3c]<0xd0 ? 0x20+config[0x3c] : 0;
        if (irq!=expected_irq) return fail("irq-firmware-route");
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
