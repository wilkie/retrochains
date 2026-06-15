int twice(int x);
int main(void) {
  int a;
  int b;
  a = 5;
  b = 7;
  return twice(a > b ? a : b);
}
