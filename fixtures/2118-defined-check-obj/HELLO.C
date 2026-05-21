#define HAS_FEATURE
int main(void) {
  int x = 0;
#if defined(HAS_FEATURE)
  x = 100;
#endif
#if !defined(MISSING)
  x = x + 5;
#endif
  return x;
}
