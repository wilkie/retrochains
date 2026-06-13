int g;
int main(void) {
  int *p = &g;
  *p = 77;
  return g;
}
