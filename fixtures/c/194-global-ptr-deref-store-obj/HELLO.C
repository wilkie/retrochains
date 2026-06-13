int g;
int *p = &g;
int main(void) {
  *p = 42;
  return g;
}
