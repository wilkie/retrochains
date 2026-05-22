int twice(int x) { return x + x; }
int main(void) {
  int i;
  int a;
  i = 10;
  a = twice(i--);
  return a + i;
}
