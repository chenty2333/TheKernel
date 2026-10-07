#include <stdio.h>
volatile int watched;
__attribute__((noinline)) int leaf(int input) {
    int local = input + 3;
    watched = local;
    return local;
}
__attribute__((noinline)) int middle(int input) {
    int answer = leaf(input);
    return answer * 2;
}
int main(void) {
    int result = middle(4);
    printf("DEBUG_RESULT=%d\n", result);
    return (result == 14 || result == 24) ? 0 : 1;
}
