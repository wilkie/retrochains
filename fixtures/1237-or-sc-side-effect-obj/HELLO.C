int main(void) {
  int a = 0;
  int b = 5;
  if (a || ++b) return b;
  return 0;
}
