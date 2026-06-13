int two(void) {
  return 2;
}
int main(void) {
  int a = 10;
  a -= two();
  return a;
}
