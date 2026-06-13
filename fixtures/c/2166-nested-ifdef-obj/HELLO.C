#define LEVEL 2
int main(void) {
#if LEVEL > 0
  int x = 10;
  #if LEVEL > 1
    x += 100;
    #if LEVEL > 2
      x += 1000;
    #endif
  #endif
  return x;
#else
  return 0;
#endif
}
