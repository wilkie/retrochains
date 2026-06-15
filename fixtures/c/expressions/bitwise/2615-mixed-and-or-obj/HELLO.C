int main(void) {
  int a;
  int b;
  int c;
  a = 1;
  b = 0;
  c = 1;
  if (a && (b || c)) return 7;
  return 0;
}
