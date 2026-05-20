int main(void) {
  char c;
  char *p;
  c = 0;
  p = &c;
  *p = 42;
  return c;
}
