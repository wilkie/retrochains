int main(void) {
  static char *a = "hello";
  static char *b = "hello";
  return a == b;
}
