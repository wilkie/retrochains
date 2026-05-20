void f(char *p) {
  *p = 7;
}
int main(void) {
  char c = 0;
  f(&c);
  return c;
}
