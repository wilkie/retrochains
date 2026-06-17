int g;
int main(void) {
  int *p;
  p = &g;
  *p = 5;
  return g;
}
