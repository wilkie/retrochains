int g;
int main(void) {
  int *p = &g;
  *p = 42;
  return g;
}
