#define DEBUG
int main(void) {
  int x = 10;
#ifdef DEBUG
  x = x + 1;
#else
  x = x - 1;
#endif
  return x;
}
