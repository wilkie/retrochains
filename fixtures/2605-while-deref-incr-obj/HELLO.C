char buf[5] = "abc";
int main(void) {
  char *p;
  int n;
  p = buf;
  n = 0;
  while (*p) {
    p = p + 1;
    n = n + 1;
  }
  return n;
}
