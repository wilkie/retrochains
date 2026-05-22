int counter(void) {
  static int n;
  n = n + 1;
  return n;
}
int main(void) {
  counter();
  counter();
  return counter();
}
