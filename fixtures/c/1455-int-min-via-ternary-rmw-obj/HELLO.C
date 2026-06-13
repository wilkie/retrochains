int main(void) {
  int a = 5;
  int b = 3;
  a -= a < b ? 0 : a - b;
  return a;
}
