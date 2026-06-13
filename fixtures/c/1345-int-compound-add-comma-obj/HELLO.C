int main(void) {
  int a = 5;
  int b = 0;
  a += (b = 3, b + 1);
  return a;
}
