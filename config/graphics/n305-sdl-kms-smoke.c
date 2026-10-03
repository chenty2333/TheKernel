#include <SDL.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/kd.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>
static int terminal = -1, old_mode = KD_TEXT;
static int restore(void) { int result=0; if(terminal>=0) { result=ioctl(terminal,KDSETMODE,old_mode); close(terminal); terminal=-1; } return result; }
static void interrupted(int sig) { restore(); _exit(128+sig); }
int main(void) {
    int result=1;
    terminal=open("/dev/tty0",O_RDWR|O_CLOEXEC);
    if(terminal<0 || ioctl(terminal,KDGETMODE,&old_mode)<0) { perror("SDL KMS terminal"); restore(); return 1; }
    signal(SIGINT,interrupted); signal(SIGTERM,interrupted);
    /* A raw KMS client owns graphics while running. Do not let serial stdout
       trigger the firmware fbcon to copy its old text buffer over our frame. */
    if(ioctl(terminal,KDSETMODE,KD_GRAPHICS)<0) { perror("SDL KMS KD_GRAPHICS"); restore(); return 1; }
    SDL_Window *window=NULL; SDL_Renderer *renderer=NULL;
    if(SDL_Init(SDL_INIT_VIDEO)<0) { fprintf(stderr,"SDL init: %s\n",SDL_GetError()); goto done; }
    if(!SDL_GetCurrentVideoDriver() || strcmp(SDL_GetCurrentVideoDriver(),"KMSDRM")) { fprintf(stderr,"unexpected SDL driver: %s\n",SDL_GetCurrentVideoDriver()); goto done; }
    window=SDL_CreateWindow("N305 KMS smoke",SDL_WINDOWPOS_UNDEFINED,SDL_WINDOWPOS_UNDEFINED,800,600,SDL_WINDOW_FULLSCREEN_DESKTOP);
    if(!window) { fprintf(stderr,"SDL window: %s\n",SDL_GetError()); goto done; }
    renderer=SDL_CreateRenderer(window,-1,0);
    if(!renderer) { fprintf(stderr,"SDL renderer: %s\n",SDL_GetError()); goto done; }
    SDL_RendererInfo info; SDL_GetRendererInfo(renderer,&info);
    fprintf(stderr,"N305 SDL renderer=%s\n",info.name);
    if(ioctl(terminal,KDSETMODE,KD_GRAPHICS)<0) goto done;
    SDL_SetRenderDrawColor(renderer,255,0,0,255);
    /* Exercise repeated page flips, as an SDL game does, not just its first
       EGL/GBM buffer. The external pixel check remains the display oracle. */
    Uint64 start=SDL_GetTicks64(); int announced=0;
    do {
        if(SDL_RenderClear(renderer)<0) goto done;
        SDL_RenderPresent(renderer);
        if(!announced && SDL_GetTicks64()-start>=1000) {
            puts("N305_SDL_KMS_FRAME_READY driver=KMSDRM expected=red"); fflush(stdout); announced=1;
        }
        SDL_Delay(16);
    } while(SDL_GetTicks64()-start<12000);
    result=0;
done:
    if(renderer) SDL_DestroyRenderer(renderer);
    if(window) SDL_DestroyWindow(window);
    SDL_Quit();
    int restored=restore()==0;
    if(!restored) result=1;
    printf("N305_SDL_KMS_EXIT status=%d %s\n",result,restored ? "console_restored" : "console_restore_failed");
    return result;
}
