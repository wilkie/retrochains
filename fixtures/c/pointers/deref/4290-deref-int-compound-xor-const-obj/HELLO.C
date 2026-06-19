int main() {
  int x;
  int *p;
  x = 100;
  p = &x;
  *p ^= 6;
  return x;
}
