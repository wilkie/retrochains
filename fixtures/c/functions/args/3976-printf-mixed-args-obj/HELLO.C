int printf(char *, ...);
int main(void) {
  int x = 42;
  char *s = "hi";
  printf("%s %d\n", s, x);
  return 0;
}
