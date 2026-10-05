#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

static void need(int ok,const char *what) {if(!ok){fprintf(stderr,"block-statistics: %s errno=%d (%s)\n",what,errno,strerror(errno));exit(1);}}
struct record {unsigned major,minor;char name[128];unsigned long long field[17];};
static int table(const char *wanted,struct record *found) {
    FILE *file=fopen("/proc/diskstats","r");need(file!=NULL,"diskstats open");char line[1024];int rows=0,match=0;
    while(fgets(line,sizeof(line),file)) {
        struct record row={0};int offset=0;
        need(sscanf(line,"%u %u %127s%n",&row.major,&row.minor,row.name,&offset)==3,"diskstats identity");
        char *at=line+offset;
        for(int field=0;field<17;field++) {while(*at==' ')at++;char *end;errno=0;row.field[field]=strtoull(at,&end,10);need(end!=at && !errno,"diskstats decimal fields");at=end;}
        need(!strcmp(at,"\n"),"diskstats exact field count/newline");rows++;
        if(wanted && !strcmp(wanted,row.name)){*found=row;match++;}
    }
    need(!ferror(file) && !fclose(file) && rows>0,"diskstats rows/read");return match;
}
static void source_agreement(const char *name) {
    for(int retry=0;retry<100;retry++) {
        struct record proc;need(table(name,&proc)==1,"diskstats selected device");
        char path[256];snprintf(path,sizeof(path),"/sys/class/block/%s/stat",name);
        FILE *file=fopen(path,"r");need(file!=NULL,"sysfs stat open");unsigned long long fields[17];
        for(int field=0;field<17;field++)need(fscanf(file,"%llu",&fields[field])==1,"sysfs stat fields");
        need(fgetc(file)=='\n' && fgetc(file)==EOF && !fclose(file),"sysfs stat exact EOF");
        if(!memcmp(proc.field,fields,sizeof(fields)))return;
        usleep(10000);
    }
    need(0,"quiescent common-source agreement");
}
static void real_iostat(void) {
    int output[2];need(!pipe(output),"iostat pipe");pid_t child=fork();need(child>=0,"iostat fork");
    if(!child){close(output[0]);dup2(output[1],STDOUT_FILENO);dup2(output[1],STDERR_FILENO);close(output[1]);setenv("S_COLORS","never",1);
        execl("/opt/thekernel-tools/bin/iostat","iostat","-dx",(char *)NULL);_exit(127);}
    close(output[1]);char text[32768];size_t used=0;ssize_t bytes;
    while(used<sizeof(text)-1 && (bytes=read(output[0],text+used,sizeof(text)-1-used))>0)used+=bytes;
    need(used<sizeof(text)-1,"iostat output capacity");text[used]=0;close(output[0]);int status;
    need(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"actual iostat exit");
    printf("BLOCK_IOSTAT\n%s",text);need(strstr(text,"Device") && strstr(text,"vda") && !strstr(text,"Cannot") && !strstr(text,"Error"),"actual iostat root disk row without diagnostics");
}
int main(int argc,char **argv) {
    if(argc==2 && !strcmp(argv[1],"--shape")){table(NULL,NULL);puts("BLOCK_STATISTICS_SHAPE_OK");return 0;}
    int tools=argc==2 && !strcmp(argv[1],"--tools");need(argc==1 || tools,"arguments");
    struct record before,after;need(table("vda",&before)==1,"root disk row");source_agreement("vda");
    int fd=open("/dev/vda",O_RDONLY|O_CLOEXEC|O_DIRECT);need(fd>=0,"read-only root device");struct stat metadata;
    need(!fstat(fd,&metadata) && S_ISBLK(metadata.st_mode) && major(metadata.st_rdev)==before.major && minor(metadata.st_rdev)==before.minor,"actual device identity");
    void *buffer;need(!posix_memalign(&buffer,4096,4096),"aligned read buffer");
    for(int request=0;request<20;request++)need(pread(fd,buffer,4096,request*4096)==4096,"actual root block read");
    free(buffer);close(fd);need(table("vda",&after)==1,"post-read row");
    need(after.field[0]>=before.field[0]+20 && after.field[2]>=before.field[2]+160,"actual reads advance completions and 512-byte sectors");
    source_agreement("vda");if(tools)real_iostat();
    printf("BLOCK_STATISTICS_OK read_delta=%llu sector_delta=%llu\n",after.field[0]-before.field[0],after.field[2]-before.field[2]);return 0;
}
