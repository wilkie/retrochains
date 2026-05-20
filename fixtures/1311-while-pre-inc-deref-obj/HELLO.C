int main(void) {
  char *p;
  int n;
  p = "ab";
  n = 0;
  while (*++p) n++;
  return n;
}
