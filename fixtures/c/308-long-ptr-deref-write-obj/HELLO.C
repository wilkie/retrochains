long g;
int main(void) {
  long *p = &g;
  *p = 42;
  return 0;
}
