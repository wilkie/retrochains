int two(void) {
  return 2;
}
int main(void) {
  int a = 5;
  a += two() + 3;
  return a;
}
