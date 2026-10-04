/* Linux ALSA UAPI exerciser. No OSS adapter or kernel-private ABI. */
#include <sys/ioctl.h>
#include <sound/asound.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#define CHECK(x) do { if (!(x)) { perror(#x); return 1; } } while (0)
static void any(struct snd_pcm_hw_params *p) {
    memset(p, 0, sizeof(*p));
    memset(p->masks, 255, sizeof(p->masks));
    for (unsigned i = 0; i < sizeof(p->intervals)/sizeof(p->intervals[0]); ++i)
        p->intervals[i].max = UINT32_MAX;
    p->rmask = UINT32_MAX;
    p->info = UINT32_MAX;
}
int main(void) {
    int ctl = open("/dev/snd/controlC0", O_RDWR);
    CHECK(ctl >= 0);
    struct snd_ctl_card_info card;
    CHECK(ioctl(ctl, SNDRV_CTL_IOCTL_CARD_INFO, &card) == 0);
    CHECK(card.card == 0);
    close(ctl);
    int fd = open("/dev/snd/pcmC0D0p", O_RDWR | O_NONBLOCK);
    CHECK(fd >= 0);
    errno = 0;
    CHECK(open("/dev/dsp", O_WRONLY) < 0 && errno == EBUSY);
    struct snd_pcm_hw_params p;
    any(&p);
    p.masks[1].bits[0] = 1u << SNDRV_PCM_FORMAT_U8;
    memset(&p.masks[1].bits[1], 0, sizeof(p.masks[1].bits) - 4);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_HW_REFINE, &p) < 0 && errno == EINVAL);
    any(&p);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_HW_PARAMS, &p) == 0);
    struct snd_pcm_sw_params sw = {0};
    sw.avail_min = 1024;
    sw.start_threshold = 8192; /* force explicit START */
    sw.stop_threshold = 4096;
    sw.boundary = 8192;
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_SW_PARAMS, &sw) == 0);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_PREPARE) == 0);
    int16_t samples[2048] = {0};
    struct snd_xferi x = {.buf = samples, .frames = 1024};
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_WRITEI_FRAMES, &x) == 0 && x.result == 1024);
    struct snd_pcm_sync_ptr sync = {.flags = SNDRV_PCM_SYNC_PTR_APPL | SNDRV_PCM_SYNC_PTR_AVAIL_MIN};
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_SYNC_PTR, &sync) == 0);
    CHECK(sync.s.status.state == SNDRV_PCM_STATE_PREPARED && sync.s.status.hw_ptr == 0);
    CHECK(sync.c.control.appl_ptr == 1024);
    sync.flags = SNDRV_PCM_SYNC_PTR_AVAIL_MIN;
    sync.c.control.appl_ptr = 999;
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_SYNC_PTR, &sync) < 0 && errno == EINVAL);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_START) == 0);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_DROP) == 0);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_PREPARE) == 0);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_DROP) == 0);
    CHECK(ioctl(fd, SNDRV_PCM_IOCTL_HW_FREE) == 0);
    close(fd);
    puts("ALSA_NATIVE_ABI_OK");
    return 0;
}
