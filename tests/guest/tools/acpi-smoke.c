/* Original TheKernel ACPI userspace acceptance, Apache-2.0. */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <dirent.h>
static void require(int ok,const char *what){if(!ok){perror(what);exit(1);}}
int main(int argc,char **argv){
    FILE *command=fopen("/proc/cmdline","r");char line[4096]={0};require(command!=NULL,"cmdline");fgets(line,sizeof(line),command);fclose(command);
    int enabled=1;char *save=NULL;for(char *word=strtok_r(line," \t\r\n",&save);word;word=strtok_r(NULL," \t\r\n",&save)){if(!strncmp(word,"acpi=",5))enabled=!strcmp(word+5,"acpica");}
    if(!enabled){puts("THEKERNEL_ACPI_STATIC_FALLBACK_OK");return 0;}
    struct stat st;require(stat("/sys/firmware/acpi/tables/DSDT",&st)==0,"DSDT stat");require((st.st_mode&0777)==0400,"root-only table mode");
    int fd=open("/sys/firmware/acpi/tables/DSDT",O_RDONLY);require(fd>=0,"DSDT read");unsigned char bytes[1024*1024];size_t used=0;ssize_t n;
    while((n=read(fd,bytes+used,sizeof(bytes)-used))>0){used+=(size_t)n;}
    close(fd);require(n==0&&used>=36,"complete table read");require(!memcmp(bytes,"DSDT",4),"DSDT signature");uint32_t declared;memcpy(&declared,bytes+4,4);require(declared==used,"DSDT declared length");unsigned sum=0;for(size_t i=0;i<used;i++)sum+=bytes[i];require((sum&255)==0,"DSDT checksum");
    pid_t pid=fork();require(pid>=0,"fork");if(pid==0){require(setuid(65534)==0,"setuid");int denied=open("/sys/firmware/acpi/tables/DSDT",O_RDONLY);_exit(denied<0?0:1);}int status;require(waitpid(pid,&status,0)==pid&&WIFEXITED(status)&&WEXITSTATUS(status)==0,"unprivileged table denied");
    DIR *dir=opendir("/sys/bus/acpi/devices");require(dir!=NULL,"ACPI device directory");int devices=0;struct dirent *entry;while((entry=readdir(dir)))if(entry->d_name[0]!='.')devices++;closedir(dir);require(devices>0,"ACPI namespace devices");
    FILE *aml=fopen("/tmp/thekernel-acpi-dsdt.aml","wb");require(aml!=NULL,"AML output");require(fwrite(bytes,1,used,aml)==used,"AML write");fclose(aml);
    int dump=access("/opt/thekernel-tests/bin/acpidump",X_OK)==0,iasl=access("/opt/thekernel-tests/bin/iasl",X_OK)==0;
    int required=argc==2&&!strcmp(argv[1],"require-tools");
    if(dump||iasl||required){
        require(dump&&iasl,"complete inspection payload");
        require(system("/opt/thekernel-tests/bin/acpidump -s >/tmp/thekernel-acpi-headers.txt")==0,"acpidump headers");
        require(system("/opt/thekernel-tests/bin/iasl -d /tmp/thekernel-acpi-dsdt.aml >/tmp/thekernel-iasl.txt 2>&1")==0,"iasl disassembly");
        printf("THEKERNEL_ACPI_USER_TOOLS_OK devices=%d table_bytes=%zu\n",devices,used);
    }else{puts("THEKERNEL_ACPI_TOOLS_NOT_STAGED");}
    if(argc==2&&!strcmp(argv[1],"dump-q35")){
        require(!memcmp(bytes+10,"BOCHS ",6),"only QEMU-authored AML may be printed");
        printf("TK_Q35_DSDT_BEGIN %zu\n",used);for(size_t i=0;i<used;i++){printf("%02x",bytes[i]);if((i&31)==31)putchar('\n');}puts("\nTK_Q35_DSDT_END");
    }
    printf("THEKERNEL_ACPI_EXPORT_OK devices=%d table_bytes=%zu\n",devices,used);return 0;
}
