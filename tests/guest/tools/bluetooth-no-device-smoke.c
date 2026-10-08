#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

#ifndef AF_BLUETOOTH
#define AF_BLUETOOTH 31
#endif
#define BTPROTO_HCI 1
#define HCI_DEV_NONE 0xffff
#define HCI_CHANNEL_MONITOR 2
#define HCIDEVUP _IOW('H', 201, int)
#define HCIDEVDOWN _IOW('H', 202, int)
#define HCIGETDEVLIST _IOR('H', 210, int)
#define HCIGETDEVINFO _IOR('H', 211, int)

struct sockaddr_hci {
    sa_family_t hci_family;
    uint16_t hci_dev;
    uint16_t hci_channel;
};
struct hci_dev_req { uint16_t dev_id; uint32_t dev_opt; };
struct hci_dev_list_req { uint16_t dev_num; struct hci_dev_req dev_req[1]; };
struct hci_dev_stats {
    uint32_t err_rx, err_tx, cmd_tx, evt_rx, acl_tx, acl_rx, sco_tx, sco_rx;
    uint32_t byte_rx, byte_tx;
};
struct hci_dev_info {
    uint16_t dev_id;
    char name[8];
    uint8_t bdaddr[6];
    uint32_t flags;
    uint8_t type;
    uint8_t features[8];
    uint8_t reserved[3];
    uint32_t pkt_type, link_policy, link_mode;
    uint16_t acl_mtu, acl_pkts, sco_mtu, sco_pkts;
    struct hci_dev_stats stat;
};

static int fail(const char *what) { perror(what); return 1; }

int main(void) {
    int fd = socket(AF_BLUETOOTH, SOCK_RAW | SOCK_CLOEXEC, BTPROTO_HCI);
    if (fd < 0) return fail("socket(AF_BLUETOOTH)");
    struct sockaddr_hci monitor = { AF_BLUETOOTH, HCI_DEV_NONE, HCI_CHANNEL_MONITOR };
    if (bind(fd, (struct sockaddr *)&monitor, sizeof(monitor)) != 0) return fail("bind(HCI monitor)");

    struct hci_dev_list_req list;
    memset(&list, 0, sizeof(list));
    list.dev_num = 1;
    if (ioctl(fd, HCIGETDEVLIST, &list) != 0) return fail("HCIGETDEVLIST");
    if (list.dev_num != 0) { fprintf(stderr, "unexpected HCI devices: %u\n", list.dev_num); return 1; }

    errno = 0;
    if (ioctl(fd, HCIDEVUP, 0) != -1 || errno != ENODEV) {
        fprintf(stderr, "HCIDEVUP without hci0: result/errno=%d/%d\n", errno == 0, errno); return 1;
    }
    struct hci_dev_info info;
    memset(&info, 0, sizeof(info));
    info.dev_id = 0;
    errno = 0;
    if (ioctl(fd, HCIGETDEVINFO, &info) != -1 || errno != ENODEV) {
        fprintf(stderr, "HCIGETDEVINFO without hci0: errno=%d\n", errno); return 1;
    }
    struct stat st;
    if (stat("/sys/class/bluetooth", &st) != 0 || !S_ISDIR(st.st_mode)) return fail("/sys/class/bluetooth");
    close(fd);
    puts("BLUETOOTH_NO_DEVICE_ACCEPTANCE_DONE");
    return 0;
}
