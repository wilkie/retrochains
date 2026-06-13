int dbl(int x) { return x + x; }
int main(void) {
  int i;
  int a;
  i = 5;
  a = dbl(++i);
  return a + i;
}
