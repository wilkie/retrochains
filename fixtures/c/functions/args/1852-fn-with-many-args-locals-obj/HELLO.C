int do_calc(int a, int b, int c, int d) {
  int x = a + b;
  int y = c + d;
  return x + y + a + d;
}
int main(void) {
  return do_calc(1, 2, 3, 4);
}
