int g;
int main(void) {
  int a;
  int b;
  g = (a = 1, b = 2, a + b);
  return 0;
}
