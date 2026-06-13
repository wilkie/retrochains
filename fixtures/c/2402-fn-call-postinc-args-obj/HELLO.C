int add(int a, int b) { return a + b; }
int main(void) {
  int i;
  int j;
  i = 10;
  j = 20;
  return add(i++, j--) + i + j;
}
