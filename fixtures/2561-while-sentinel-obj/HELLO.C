char buf[5] = "hi";
int main(void) {
  char *p;
  int n;
  p = buf;
  n = 0;
  while (*p != 0) {
    n = n + 1;
    p = p + 1;
  }
  return n;
}
