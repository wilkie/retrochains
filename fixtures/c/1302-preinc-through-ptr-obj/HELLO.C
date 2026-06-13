int g;
int main(void) {
  int *p;
  p = &g;
  *p = 5;
  ++(*p);
  return g;
}
