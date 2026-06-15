int main(void) {
  char *p;
  int n;
  p = "abc";
  n = 0;
  while (*p) {
    n++;
    p++;
  }
  return n;
}
