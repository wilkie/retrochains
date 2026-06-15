int main(void) {
  int x;
  int *p;
  int **pp;
  x = 0;
  p = &x;
  pp = &p;
  **pp = 99;
  return x;
}
