int g;
int main(void) {
  int *p = &g;
  *p = 99;
  return g;
}
