char c[4];
int main(void) {
  char *p;
  p = c;
  *++p = 7;
  return c[1];
}
