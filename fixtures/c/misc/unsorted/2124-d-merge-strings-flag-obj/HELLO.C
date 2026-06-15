int main(void) {
  static char *a = "hello";
  static char *b = "hello";
  return (a == b) ? 1 : 0;
}
