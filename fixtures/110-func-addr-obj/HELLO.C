int f(void) { return 1; }
int main(void) {
  int (*p)(void) = f;
  return p();
}
