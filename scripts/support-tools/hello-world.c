#include <unistd.h>

int main(void)
{
    static const char msg[] = "hello world";
    return write(STDOUT_FILENO, msg, sizeof(msg) - 1) == (ssize_t)(sizeof(msg) - 1) ? 0 : 1;
}
