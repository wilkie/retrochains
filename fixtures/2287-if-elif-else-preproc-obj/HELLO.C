#define MODE 2
int main(void) {
#if MODE == 1
  return 10;
#elif MODE == 2
  return 20;
#elif MODE == 3
  return 30;
#else
  return 0;
#endif
}
