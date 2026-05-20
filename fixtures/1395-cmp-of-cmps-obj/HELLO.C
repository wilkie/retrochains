int main(void) {
  int a = 5;
  int b = 5;
  int c = 10;
  if ((a == b) == (b < c)) return 1;
  return 0;
}
