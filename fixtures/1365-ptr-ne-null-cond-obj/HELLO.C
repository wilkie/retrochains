int main(void) {
  int x = 5;
  int *p;
  p = &x;
  if (p != 0) return *p;
  return -1;
}
