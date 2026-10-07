#include <stdio.h>
volatile int watched;
int main(void) { watched = 7; watched = 19; printf("WATCH_RESULT=%d\n", watched); return watched != 19; }
