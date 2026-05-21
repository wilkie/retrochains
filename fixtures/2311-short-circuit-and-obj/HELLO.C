int side_effect(int *p) { *p = 99; return 1; }
int main(void) {
  int x = 0;
  int r = (0 && side_effect(&x));
  return r * 1000 + x;
}
